// gantry — per-run cgroup cap for gantry-spawned local runs (plan Component 6).
//
// Tier-0 (zero-config mode) makes gantry "a pure local cap-wrapper" (plan
// §"Tier-0"): an intercepted subcommand never goes remote, but it must still
// land under the configured `local.cpu_quota_pct` / `local.memory_max` cgroup
// cap, exactly as the predecessor bash pair capped its local runs. This module
// is that wrapper: children spawn under
//
//   systemd-run --scope --user -p CPUQuota={n}% -p MemoryMax={m} \
//               -p MemorySwapMax=0 -q -- <real> <args…>
//
// and where a scope cannot be created — no user session, a container, no
// systemd at all — the degrade is a plain spawn with a one-per-process
// `[gantry] cap:` note (plan Component 6 "on failure to create a scope, plain
// exec"; failure-modes table "systemd-run unavailable → plain exec
// (documented: containers self-limit)"). A broken gantry must never block
// builds, so an unusable scope costs a loud note, never the run.
//
// The scope decision is made once per process, before the first spawn, by
// PROBING with the caller's own configured values: the probe validates the
// exact launch line the real child will use, so an invalid `memory_max` (or a
// manager that rejects `MemorySwapMax=0`) degrades at probe time instead of
// failing the real run. Trusting the probe also keeps exit-code fidelity
// (INV-3): once scoped, `systemd-run --scope` propagates the child's exit
// status verbatim, and a late failure is a genuine infra error, not a
// misread cap failure that would tempt an uncapped rerun.
//
// The wrapper is deliberately spawn-shaped — `(real, args) -> ExitStatus` —
// so every local tail (Tier-0, kill switch, and later the infra fallback and
// passthrough capping) keeps its existing verdict mapping untouched, and the
// boxwide `gantry.slice` placement can layer on top of the same seam.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::config::Config;

/// The process-wide per-run cap. A shim process executes exactly one
/// intercepted command, but the local-exec seam is shared (Tier-0, kill
/// switch, and later the infra fallback — plan Component 6), so the probe
/// and its `[gantry] cap:` note are paid once per PROCESS, not once per
/// tail. Config values are process-global (loaded once in the shim's entry),
/// so the first caller's view is every caller's view.
static PROCESS_CAP: OnceLock<Cap> = OnceLock::new();

/// The process-wide cap, built from `config` on first use (bf-139). Local
/// tails share this instance so the scope decision — including its probe
/// cost and one-line note — happens exactly once per process.
pub fn process_cap(config: &Config) -> &'static Cap {
    PROCESS_CAP.get_or_init(|| Cap::from_config(config))
}

/// How long the once-per-process scope probe may take before it is killed
/// and treated as unavailable. A wedged user manager must cost the run a
/// bounded, loud degrade — not a hang.
const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// Which spawn mode the once-per-process decision settled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Children spawn under a systemd-run scope with the configured caps.
    Scoped,
    /// Children spawn plain: the scope is unavailable (or was refused), the
    /// documented degrade.
    Plain,
}

/// The once-per-process outcome of probing the scope mechanism.
#[derive(Debug)]
struct Decision {
    mode: Mode,
    /// The resolved `systemd-run` binary — `Some` exactly when `mode` is
    /// [`Mode::Scoped`] (the probe only passes through a real binary).
    systemd_run: Option<PathBuf>,
    /// The configured cap values, carried for [`Cap::describe`].
    cpu_quota_pct: u32,
    memory_max: String,
}

/// Per-run cgroup cap for gantry-spawned local runs (plan Component 6).
///
/// Build with [`Self::from_config`]; spawn children with [`Self::spawn`].
/// The first spawn (or [`Self::describe`]) pays the one-time probe; every
/// later spawn reuses the cached decision. When scoped, the child runs under
/// `CPUQuota` / `MemoryMax` / `MemorySwapMax=0`; when plain, a single
/// `[gantry] cap:` note explains the degrade. Either way [`Self::spawn`]
/// returns the child's own [`ExitStatus`] — the `--scope` wrapper waits for
/// the child and propagates its code — so callers keep their INV-3 mapping.
pub struct Cap {
    cpu_quota_pct: u32,
    memory_max: String,
    /// Test-only override of the resolved `systemd-run` path: `Some(None)`
    /// forces the no-binary path, `Some(Some(path))` pins a (usually fake)
    /// binary. `None` means "search PATH", the production behavior.
    #[cfg(test)]
    systemd_run_override: Option<Option<PathBuf>>,
    decision: OnceLock<Decision>,
}

impl Cap {
    /// Read the cap configuration from the layered config (Tier-0 defaults
    /// when no config exists: 200% CPU, 6G memory).
    pub fn from_config(config: &Config) -> Self {
        Self {
            cpu_quota_pct: u32::from(config.local.cpu_quota_pct),
            memory_max: config.local.memory_max.clone(),
            #[cfg(test)]
            systemd_run_override: None,
            decision: OnceLock::new(),
        }
    }

    /// Test constructor pinning the `systemd-run` resolution and cap values.
    #[cfg(test)]
    fn with_parts(cpu_quota_pct: u32, memory_max: &str, systemd_run: Option<PathBuf>) -> Self {
        Self {
            cpu_quota_pct,
            memory_max: memory_max.to_string(),
            systemd_run_override: Some(systemd_run),
            decision: OnceLock::new(),
        }
    }

    /// Decide (once per process) how local children spawn, printing the
    /// cap/degrade note exactly once.
    fn decide(&self) -> &Decision {
        self.decision.get_or_init(|| {
            #[cfg(test)]
            let resolved = match &self.systemd_run_override {
                Some(over) => over.clone(),
                None => self.find_systemd_run(),
            };
            #[cfg(not(test))]
            let resolved = self.find_systemd_run();
            let (decision, note) =
                decide_with(resolved, self.cpu_quota_pct, &self.memory_max, &probe_scope);
            if let Some(note) = note {
                eprintln!("[gantry] cap: {note}");
            }
            decision
        })
    }

    /// Search PATH for `systemd-run` (production resolution; tests inject).
    fn find_systemd_run(&self) -> Option<PathBuf> {
        let path = std::env::var("PATH").ok()?;
        path.split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| PathBuf::from(dir).join("systemd-run"))
            .find(|candidate| is_executable_file(candidate))
    }

    /// Spawn `real` with `args`, under the configured cgroup cap when the
    /// scope is available, plainly otherwise (module docs). The child's own
    /// status comes back either way, so the caller's verdict mapping is
    /// untouched by this wrapper.
    pub fn spawn(&self, real: &Path, args: &[String]) -> std::io::Result<ExitStatus> {
        let decision = self.decide();
        match &decision.systemd_run {
            Some(systemd_run) => {
                let mut cmd = Command::new(systemd_run);
                cmd.args(scope_flags(decision.cpu_quota_pct, &decision.memory_max));
                cmd.arg(real).args(args);
                cmd.status()
            }
            None => Command::new(real).args(args).status(),
        }
    }

    /// Human-readable description of the active cap tier, for `gantry
    /// quickcheck` and `doctor`-style reporting (plan R5: "doctor reports
    /// active cap tier"). Triggers the one-time probe if not yet made.
    pub fn describe(&self) -> String {
        let decision = self.decide();
        match decision.mode {
            Mode::Scoped => format!(
                "systemd-run scope (CPUQuota={}%, MemoryMax={})",
                decision.cpu_quota_pct, decision.memory_max
            ),
            Mode::Plain => "plain exec — no cgroup cap (documented degrade)".to_string(),
        }
    }
}

/// The flags that turn a `systemd-run` invocation into the configured cap,
/// up to and including the `--` separator. Pure so the argv contract is
/// unit-testable without a manager.
fn scope_flags(cpu_quota_pct: u32, memory_max: &str) -> Vec<String> {
    [
        "--scope".to_string(),
        "--user".to_string(),
        "-p".to_string(),
        format!("CPUQuota={cpu_quota_pct}%"),
        "-p".to_string(),
        format!("MemoryMax={memory_max}"),
        "-p".to_string(),
        "MemorySwapMax=0".to_string(),
        "-q".to_string(),
        "--".to_string(),
    ]
    .into_iter()
    .collect()
}

/// Resolve the once-per-process decision from a `systemd-run` resolution.
///
/// Returns the decision plus the note to print (if any). `probe` is the
/// scope probe, parameterized so tests can sit behind a fake binary.
fn decide_with(
    systemd_run: Option<PathBuf>,
    cpu_quota_pct: u32,
    memory_max: &str,
    probe: &dyn Fn(&Path, u32, &str) -> Result<(), String>,
) -> (Decision, Option<String>) {
    let Some(systemd_run) = systemd_run else {
        return (
            Decision {
                mode: Mode::Plain,
                systemd_run: None,
                cpu_quota_pct,
                memory_max: memory_max.to_string(),
            },
            Some(
                "systemd-run not found on PATH (typical in containers/CI) — \
                 local runs spawn plain, without a cgroup cap \
                 (documented degrade, plan §6)"
                    .to_string(),
            ),
        );
    };

    match probe(&systemd_run, cpu_quota_pct, memory_max) {
        Ok(()) => (
            Decision {
                mode: Mode::Scoped,
                systemd_run: Some(systemd_run),
                cpu_quota_pct,
                memory_max: memory_max.to_string(),
            },
            Some(format!(
                "capping local runs via systemd-run scope \
                 (CPUQuota={cpu_quota_pct}%, MemoryMax={memory_max})"
            )),
        ),
        Err(why) => (
            Decision {
                mode: Mode::Plain,
                systemd_run: None,
                cpu_quota_pct,
                memory_max: memory_max.to_string(),
            },
            Some(format!(
                "systemd-run scope unavailable ({why}) — local runs spawn \
                 plain, without a cgroup cap (documented degrade, plan §6)"
            )),
        ),
    }
}

/// Probe the scope mechanism with the caller's own configured values: run
/// `true` through the exact launch line the real child will use. Ok iff the
/// trivial child completed successfully within [`PROBE_BUDGET`]; the Err
/// carries the human-readable reason for the degrade note.
fn probe_scope(systemd_run: &Path, cpu_quota_pct: u32, memory_max: &str) -> Result<(), String> {
    probe_scope_with_budget(systemd_run, cpu_quota_pct, memory_max, PROBE_BUDGET)
}

/// [`probe_scope`] with the budget explicit — the seam tests use to pin a
/// short budget instead of waiting out the production one.
fn probe_scope_with_budget(
    systemd_run: &Path,
    cpu_quota_pct: u32,
    memory_max: &str,
    budget: Duration,
) -> Result<(), String> {
    let mut cmd = Command::new(systemd_run);
    cmd.args(scope_flags(cpu_quota_pct, memory_max));
    cmd.arg("true");
    // The probe's own output is irrelevant (success is the status); stdout is
    // discarded and stderr is drained only after the bounded wait so a chatty
    // failure can still name itself in the degrade note.
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot spawn {}: {e}", systemd_run.display()))?;
    let status =
        wait_with_timeout(&mut child, budget).map_err(|e| format!("probe wait failed: {e}"))?;
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "probe did not finish within {:.1}s",
            budget.as_secs_f32()
        ));
    };
    if status.success() {
        return Ok(());
    }
    // The child exited non-zero: drain the (small) stderr for the note. The
    // pipe holds well under its buffer bound for a `systemd-run … true`
    // failure, so reading after the wait cannot deadlock the poll loop.
    let detail = child
        .stderr
        .take()
        .and_then(|mut pipe| {
            use std::io::Read;
            let mut buf = String::new();
            pipe.read_to_string(&mut buf).ok()?;
            Some(buf)
        })
        .map(|buf| buf.trim().to_string())
        .filter(|buf| !buf.is_empty())
        .unwrap_or_else(|| format!("exited with {status}"));
    Err(detail)
}

/// Wait for `child` up to `budget`, polling so a hung child can be killed
/// rather than waited on forever (`std` has no blocking-with-timeout wait).
/// `Ok(None)` means the budget expired — the child is still running and is
/// the caller's to kill.
pub(crate) fn wait_with_timeout(
    child: &mut Child,
    budget: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Executable regular file check for the PATH walk (symlink targets included
/// via `metadata`, which follows links).
fn is_executable_file(candidate: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .map(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(candidate)
            .map(|md| md.is_file())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a fake `systemd-run` script and return its path. Scripts use
    /// `#!/usr/bin/env bash` — NixOS has no `/bin/bash`.
    fn fake_systemd_run(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("systemd-run");
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n"))
            .expect("write fake systemd-run");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake systemd-run");
        }
        path
    }

    /// A fake that validates the expected cap flags are present, then execs
    /// whatever follows `--` — proving both the argv contract and that the
    /// child's exit status propagates through the wrapper.
    const VALIDATING_EXEC: &str = r#"
cpu=""; mem=""; swap=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --scope|--user|-p|-q) ;;
    CPUQuota=*) cpu="$1" ;;
    MemoryMax=*) mem="$1" ;;
    MemorySwapMax=*) swap="$1" ;;
    --) shift; break ;;
    *) exit 9 ;;  # unexpected token
  esac
  shift
done
[ -n "$cpu" ] || { echo "missing CPUQuota" >&2; exit 3; }
[ -n "$mem" ] || { echo "missing MemoryMax" >&2; exit 4; }
[ "$swap" = "MemorySwapMax=0" ] || { echo "missing MemorySwapMax=0" >&2; exit 5; }
exec "$@"
"#;

    #[test]
    fn scope_flags_carry_the_configured_cap() {
        let flags = scope_flags(200, "6G");
        assert_eq!(
            flags,
            vec![
                "--scope",
                "--user",
                "-p",
                "CPUQuota=200%",
                "-p",
                "MemoryMax=6G",
                "-p",
                "MemorySwapMax=0",
                "-q",
                "--",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn absent_binary_degrades_to_plain_with_a_note() {
        let (decision, note) = decide_with(None, 200, "6G", &|_, _, _| {
            panic!("probe must not run without a binary")
        });
        assert_eq!(decision.mode, Mode::Plain);
        assert!(decision.systemd_run.is_none());
        let note = note.expect("degrade must carry a note");
        assert!(note.contains("systemd-run not found"), "{note}");
    }

    #[test]
    fn successful_probe_selects_the_scoped_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = fake_systemd_run(dir.path(), "exit 0");
        let (decision, note) = decide_with(Some(fake.clone()), 200, "6G", &probe_scope);
        assert_eq!(decision.mode, Mode::Scoped);
        assert_eq!(decision.systemd_run.as_deref(), Some(fake.as_path()));
        let note = note.expect("scoped decision carries the cap note");
        assert!(note.contains("CPUQuota=200%"), "{note}");
        assert!(note.contains("MemoryMax=6G"), "{note}");
    }

    #[test]
    fn failed_probe_degrades_and_names_the_reason() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = fake_systemd_run(dir.path(), "echo 'no user session' >&2; exit 1");
        let (decision, note) = decide_with(Some(fake), 200, "6G", &probe_scope);
        assert_eq!(decision.mode, Mode::Plain);
        assert!(decision.systemd_run.is_none());
        let note = note.expect("degrade must carry a note");
        assert!(note.contains("unavailable"), "{note}");
        assert!(note.contains("no user session"), "{note}");
    }

    #[test]
    fn probe_timeout_degrades_instead_of_hanging() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = fake_systemd_run(dir.path(), "sleep 30");
        // A pinned short budget: the production budget (10s) would make this
        // test wait it out, but boundedness is what is under test, and a
        // 250ms budget exercises the same expiry path.
        let budget = Duration::from_millis(250);
        let started = Instant::now();
        let (decision, note) = decide_with(Some(fake), 200, "6G", &move |path, cpu, mem| {
            probe_scope_with_budget(path, cpu, mem, budget)
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "probe must be bounded, took {:?}",
            started.elapsed()
        );
        assert_eq!(decision.mode, Mode::Plain);
        let note = note.expect("degrade must carry a note");
        assert!(note.contains("did not finish within"), "{note}");
    }

    #[test]
    fn scoped_spawn_propagates_the_child_exit_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = fake_systemd_run(dir.path(), VALIDATING_EXEC);
        let cap = Cap::with_parts(200, "6G", Some(fake));
        let status = cap
            .spawn(Path::new("sh"), &["-c".to_string(), "exit 42".to_string()])
            .expect("spawn through the fake scope");
        assert_eq!(
            status.code(),
            Some(42),
            "child code must survive the wrapper"
        );
    }

    #[test]
    fn degraded_spawn_runs_the_child_plain() {
        let dir = tempfile::TempDir::new().unwrap();
        // A fake that passes the probe (so the decision starts Scoped) cannot
        // exercise the plain path — use one that fails it, then confirm the
        // plain spawn still runs the child faithfully.
        let fake = fake_systemd_run(dir.path(), "exit 1");
        let cap = Cap::with_parts(200, "6G", Some(fake));
        let status = cap
            .spawn(Path::new("sh"), &["-c".to_string(), "exit 7".to_string()])
            .expect("plain spawn");
        assert_eq!(status.code(), Some(7));
        assert_eq!(cap.describe(), "plain exec — no cgroup cap (documented degrade)");
    }

    #[test]
    fn describe_reports_the_scoped_tier_with_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = fake_systemd_run(dir.path(), "exit 0");
        let cap = Cap::with_parts(150, "3G", Some(fake));
        assert_eq!(
            cap.describe(),
            "systemd-run scope (CPUQuota=150%, MemoryMax=3G)"
        );
    }

    #[test]
    fn wait_with_timeout_returns_the_fast_childs_status() {
        let mut child = Command::new("true").spawn().unwrap();
        let status = wait_with_timeout(&mut child, Duration::from_secs(5))
            .unwrap()
            .expect("fast child finishes");
        assert!(status.success());
    }

    #[test]
    fn wait_with_timeout_expires_on_a_hung_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let status = wait_with_timeout(&mut child, Duration::from_millis(150)).unwrap();
        assert!(status.is_none(), "budget expiry must report None");
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn from_config_reads_the_local_cap_values() {
        let cfg = Config::tier_0_defaults();
        let cap = Cap::from_config(&cfg);
        assert_eq!(cap.cpu_quota_pct, 200);
        assert_eq!(cap.memory_max, "6G");
    }
}
