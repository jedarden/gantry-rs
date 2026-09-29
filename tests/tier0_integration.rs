// gantry — Tier-0 zero-config integration tests (bf-2pad).
//
// Proves the plan's Tier-0 contract end-to-end (plan §"Tier-0"): with NO config
// file anywhere — no /etc/gantry, no user layer, no repo .gantry.toml — an
// intercepted `cargo test` still runs, locally, with the full RunLog treatment
// (write-ahead intent + terminal verdict, INV-1), a faithful exit code (INV-3),
// nothing pushed to the remote, and the tier noted on stderr at most once per
// hour (state-file timestamp).
//
// Every test runs the real gantry binary AS the cargo shim (argv[0]="cargo" via
// a symlink, the same technique as tests/integration.rs) inside a throwaway git
// repo with a throwaway HOME, so the zero-config precondition holds regardless
// of what the invoking user happens to have configured, and the runlog /
// tier-notice timestamps land in the throwaway state dir instead of the real
// ~/.local/state/gantry.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// A self-contained Tier-0 world: a fixture cargo project that is also a git
/// repo with a bare remote, plus an isolated HOME. Nothing here is shared with
/// other tests or with the invoking user's real environment.
struct Tier0World {
    /// Isolated HOME: no user config layer, and the sandbox for the runlog
    /// (`$HOME/.local/state/gantry/runs.jsonl`) and the tier-notice timestamp.
    _home: TempDir,
    /// The fixture project directory (a git repo, no .gantry.toml).
    project: PathBuf,
    /// The `cargo` symlink inside the project that invokes gantry.
    cargo_link: PathBuf,
    /// The bare remote registered as `origin` (Tier-0 must never touch it).
    bare_remote: PathBuf,
    /// Directory put first on the fixture child's PATH, holding the one
    /// `cargo` symlink described at [`Tier0World::new`].
    realbin: PathBuf,
    /// Backs `realbin`; dropped last so the symlink outlives every run.
    _realbin_dir: TempDir,
    /// Dropped last so the temp dirs outlive every Command run against them.
    _project_dir: TempDir,
}

/// The fixture test: fails only when told to, so one compile serves both the
/// pass and fail round-trips (env vars do not trigger a rebuild — no build.rs).
const FIXTURE_LIB_RS: &str = r#"#[cfg(test)]
mod tests {
    #[test]
    fn tier0_fixture_test() {
        if std::env::var("GANTRY_TIER0_FIXTURE_FAIL").is_ok() {
            panic!("fixture instructed to fail");
        }
    }
}
"#;

impl Tier0World {
    /// Create the zero-config world: temp HOME, temp git repo with a bare
    /// `origin`, fixture crate, and the cargo→gantry symlink. Asserts the
    /// zero-config precondition as it goes — if any config layer is somehow
    /// visible from the fixture, the test fails loudly instead of proving
    /// nothing.
    fn new(name: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // Fixture crate: trivial, dependency-free, compiles in ~1s. The empty
        // `[workspace]` detaches it from any outer workspace root that cargo's
        // upward walk might find (a stray /tmp/Cargo.toml would otherwise make
        // every fixture run die with "believes it's in a workspace").
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"tier0-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        fs::create_dir(project.join("src")).expect("create fixture src/");
        fs::write(project.join("src/lib.rs"), FIXTURE_LIB_RS).expect("write fixture lib.rs");

        // The shim invocation: a symlink named `cargo` pointing at gantry, so
        // argv[0] dispatches to the cargo tool profile.
        let cargo_link = project.join("cargo");
        std::os::unix::fs::symlink(gantry_binary(), &cargo_link)
            .expect("create cargo→gantry symlink");

        // Git repo + bare remote, with identity pinned (a global gitconfig is
        // not guaranteed under the isolated HOME).
        let bare_remote = project.join(".git-state/bare-remote.git");
        fs::create_dir_all(bare_remote.parent().unwrap()).expect("create .git-state/");
        git(&project, &["init", "--bare", bare_remote.to_str().unwrap()]);
        git(&project, &["init"]);
        git(&project, &["config", "user.name", "Tier0 Test"]);
        git(&project, &["config", "user.email", "tier0@test.invalid"]);
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // Pin the "real cargo" the shim's PATH lookup resolves to the cargo
        // that compiled this very test (env!("CARGO")). The host PATH cannot
        // be trusted to yield it: where `cargo` on PATH is a wrapper that
        // offloads `cargo test` in a git repo with an origin to a CI cluster,
        // the fixture — exactly such a repo — would submit itself to that
        // cluster instead of running locally. A symlink of the real binary in
        // a directory with no other cargo keeps the resolution deterministic
        // while the host PATH tail still serves rustc, the linker, and git.
        let realbin_dir = TempDir::new().expect("create realbin dir");
        let realbin = realbin_dir.path().to_path_buf();
        std::os::unix::fs::symlink(env!("CARGO"), realbin.join("cargo"))
            .expect("symlink the real cargo into realbin");

        let world = Tier0World {
            _home: home,
            project,
            cargo_link,
            bare_remote,
            realbin,
            _realbin_dir: realbin_dir,
            _project_dir: project_dir,
        };
        world.assert_zero_config();
        world
    }

    /// Fail loudly if any config layer is visible from inside the fixture —
    /// the entire point of these tests is the NO-config precondition.
    fn assert_zero_config(&self) {
        assert!(
            !Path::new("/etc/gantry/config.toml").exists(),
            "/etc/gantry/config.toml exists on this machine; the Tier-0 \
             integration tests cannot prove the zero-config contract here"
        );
        assert!(
            !self.home().join(".config/gantry/config.toml").exists(),
            "the isolated HOME unexpectedly carries a user config layer"
        );
        assert!(
            !self.project.join(".gantry.toml").exists(),
            "the fixture repo unexpectedly carries a repo config layer"
        );
    }

    fn home(&self) -> PathBuf {
        self._home.path().to_path_buf()
    }

    /// The gantry state dir under the isolated HOME.
    fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/gantry")
    }

    /// PATH for shim children: `realbin` first — the deterministic real-cargo
    /// resolution, ahead of any host-level gantry shim dir or `cargo` wrapper
    /// — then the host PATH so rustc, the linker, and git still resolve.
    fn child_path(&self) -> String {
        format!(
            "{}:{}",
            self.realbin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Run the shim as `cargo test` in the fixture, with the isolated HOME and
    /// every GANTRY_* knob stripped, and optionally the fixture test told to
    /// fail. Returns (exit code, stdout, stderr).
    fn run_shim(&self, fail_fixture: bool) -> (i32, String, String) {
        self.run_shim_with_path_prefix(fail_fixture, None)
    }

    /// [`Self::run_shim`] with an extra directory prepended to the child's
    /// PATH — how a test injects a fake `systemd-run` to pin the cap tier
    /// deterministically (a real host user manager may or may not exist, so
    /// asserting a specific tier against the host PATH would be flaky).
    fn run_shim_with_path_prefix(
        &self,
        fail_fixture: bool,
        path_prefix: Option<&Path>,
    ) -> (i32, String, String) {
        let mut cmd = Command::new(&self.cargo_link);
        cmd.current_dir(&self.project).args(["test"]);
        // Zero-config isolation: HOME drives the user-config lookup, the
        // runlog, the state file, and the tier-notice timestamp; XDG overrides
        // would redirect the config/state layers elsewhere, so they are
        // stripped rather than set.
        cmd.env("HOME", self.home());
        let path = match path_prefix {
            Some(prefix) => format!("{}:{}", prefix.display(), self.child_path()),
            None => self.child_path(),
        };
        cmd.env("PATH", path);
        cmd.env_remove("XDG_CONFIG_HOME");
        cmd.env_remove("XDG_STATE_HOME");
        // No inherited gantry state: the Tier-0 decision must come from the
        // absence of config alone, not from a kill switch or test hook.
        cmd.env_remove("GANTRY_LOCAL");
        cmd.env_remove("GANTRY_ON");
        cmd.env_remove("GANTRY_EXEC_PATH");
        // Keep the fixture build self-contained even if the outer harness set
        // a shared target dir (concurrent same-name fixture builds would flock
        // against each other).
        cmd.env_remove("CARGO_TARGET_DIR");
        cmd.env_remove("CARGO_BUILD_TARGET_DIR");
        if fail_fixture {
            cmd.env("GANTRY_TIER0_FIXTURE_FAIL", "1");
        }
        let output = cmd.output().expect("run cargo shim");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// Run `gantry quickcheck` (the management CLI, invoked by absolute path —
    /// not the shim) inside the fixture repo, with the same zero-config
    /// isolation as the shim runs and an optional fake-`systemd-run` PATH
    /// prefix. Returns (exit code, stdout, stderr); quickcheck prints its
    /// check lines on stdout.
    fn run_quickcheck(&self, path_prefix: Option<&Path>) -> (i32, String, String) {
        let path = match path_prefix {
            Some(prefix) => format!("{}:{}", prefix.display(), self.child_path()),
            None => self.child_path(),
        };
        self.run_quickcheck_with_path(&path)
    }

    /// [`Self::run_quickcheck`] with the child PATH replaced outright — for
    /// the failed-check test, which must take `cargo` *out* of resolution
    /// entirely, and a prefix can only add directories, never remove one.
    fn run_quickcheck_with_path(&self, path: &str) -> (i32, String, String) {
        let mut cmd = Command::new(gantry_binary());
        cmd.current_dir(&self.project).arg("quickcheck");
        cmd.env("HOME", self.home());
        cmd.env("PATH", path);
        cmd.env_remove("XDG_CONFIG_HOME");
        cmd.env_remove("XDG_STATE_HOME");
        cmd.env_remove("GANTRY_LOCAL");
        cmd.env_remove("GANTRY_ON");
        cmd.env_remove("GANTRY_EXEC_PATH");
        cmd.env_remove("CARGO_TARGET_DIR");
        cmd.env_remove("CARGO_BUILD_TARGET_DIR");
        let output = cmd.output().expect("run gantry quickcheck");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// Parse the isolated runlog into JSON values, one per JSONL line.
    fn runlog_records(&self) -> Vec<serde_json::Value> {
        let path = self.state_dir().join("runs.jsonl");
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read runlog at {}: {e}", path.display()));
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("runlog line is not JSON ({e}): {line}"))
            })
            .collect()
    }

    /// All refs visible on the bare remote (empty string when none).
    fn remote_refs(&self) -> String {
        let out = Command::new("git")
            .args(["ls-remote", self.bare_remote.to_str().unwrap()])
            .output()
            .expect("git ls-remote");
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

/// Run a git subcommand in `dir`, panicking with its stderr on failure.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Write a fake `systemd-run` script into `dir` and return its path — how a
/// test pins the cap tier deterministically instead of trusting a real host
/// user manager to exist. Scripts use `#!/usr/bin/env bash` (NixOS has no
/// `/bin/bash`).
fn fake_systemd_run(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("systemd-run");
    fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).expect("write fake systemd-run");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake systemd-run");
    }
    path
}

/// A fake `systemd-run` that validates the cap launch line (the scope flags,
/// both configured cap properties, `MemorySwapMax=0`, the `--` separator) and
/// then execs whatever follows it — so both the probe's `true` and the real
/// fixture child flow through the exact argv contract, and the child's own
/// exit status survives the wrapper.
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

/// A fake `systemd-run` that rejects every invocation the way a host without
/// a usable user manager does — the documented degrade trigger (plan §6).
const REJECTING_EXEC: &str = r#"echo "no user session" >&2; exit 1"#;

/// Symlink `binary` (found on this process's PATH) into `dir`, for building a
/// restricted PATH that serves some host binaries but not others.
fn symlink_from_path(dir: &Path, binary: &str) {
    let source = std::env::var("PATH")
        .expect("PATH is set")
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(binary))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{binary} not found on PATH"));
    std::os::unix::fs::symlink(source, dir.join(binary))
        .expect("symlink binary into restricted dir");
}

/// The decision line Tier-0 prints for an intercepted subcommand.
const TIER0_DECISION: &str = "[gantry] decision: local execution (Tier-0: no backend configured)";

/// The distinguishing middle of the hourly tier notice (plan §"Tier-0").
const TIER0_NOTICE: &str = "Tier-0: no remote backend configured; running locally";

#[test]
fn no_config_intercepted_run_executes_locally_with_full_runlog_treatment() {
    let world = Tier0World::new("pass");

    let (exit_code, _stdout, stderr) = world.run_shim(false);

    // INV-3: the local run's exit code is delivered faithfully.
    assert_eq!(
        exit_code, 0,
        "passing fixture must exit 0, stderr:\n{stderr}"
    );

    // The decision names the tier, and the tier itself is noted (first run
    // within the hour → the notice must appear).
    assert!(
        stderr.contains(TIER0_DECISION),
        "stderr must carry the Tier-0 decision line, got:\n{stderr}"
    );
    assert!(
        stderr.contains(TIER0_NOTICE),
        "first run must note the tier on stderr, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "stderr must carry the verdict trailer, got:\n{stderr}"
    );

    // Every Tier-0 local run goes through the per-run cap wrapper (plan
    // §"Tier-0": "pure local cap-wrapper"): a `[gantry] cap:` line must
    // appear — the scoped note where a user manager exists, the loud
    // documented degrade where it does not. The deterministic scoped /
    // degraded variants get their own tests below; here the host decides.
    assert!(
        stderr.contains("[gantry] cap:"),
        "Tier-0 local runs must announce their cap tier, got:\n{stderr}"
    );

    // Full RunLog treatment (INV-1): a write-ahead intent naming the local
    // decision and the zero-config backend, closed by a terminal local
    // verdict — so `gantry why` can replay the decision truthfully.
    let records = world.runlog_records();
    let intents: Vec<_> = records.iter().filter(|r| r["rec"] == "intent").collect();
    let verdicts: Vec<_> = records.iter().filter(|r| r["rec"] == "verdict").collect();
    assert_eq!(intents.len(), 1, "exactly one intent record: {records:?}");
    assert_eq!(verdicts.len(), 1, "exactly one verdict record: {records:?}");
    assert_eq!(intents[0]["decision"], "local");
    assert_eq!(intents[0]["backend"], "none");
    assert!(
        intents[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Tier-0")),
        "intent reason must name Tier-0, got: {}",
        intents[0]["reason"]
    );
    assert_eq!(
        intents[0]["run_id"], verdicts[0]["run_id"],
        "intent-verdict pair"
    );
    assert_eq!(verdicts[0]["ran"], "local");
    assert_eq!(verdicts[0]["verdict"], "pass");
    assert_eq!(verdicts[0]["exit_code"], 0);

    // Tier-0 is cap-only: nothing goes remote, so the bare origin stays
    // untouched (no refs/gantry/* push, no branch mutation — a stronger shape
    // of INV-2 than "only gantry refs move").
    assert!(
        world.remote_refs().trim().is_empty(),
        "Tier-0 must never push to the remote, found: {}",
        world.remote_refs()
    );

    // The hourly notice timestamp landed in the state dir next to the runlog.
    let stamp = fs::read_to_string(world.state_dir().join("tier0-notice"))
        .expect("tier-notice timestamp written to the state dir");
    stamp
        .trim()
        .parse::<u64>()
        .expect("tier-notice timestamp is unix seconds");
}

#[test]
fn tier_notice_is_rate_limited_to_once_per_hour() {
    let world = Tier0World::new("notice");

    // First run: notice due (no timestamp file).
    let (code, _out, first) = world.run_shim(false);
    assert_eq!(code, 0, "fixture run must succeed, stderr:\n{first}");
    assert!(
        first.contains(TIER0_NOTICE),
        "first run must note the tier, got:\n{first}"
    );

    // Second run within the hour: the tier is still decided and logged, but
    // the transcript is not spammed with a second notice.
    let (code, _out, second) = world.run_shim(false);
    assert_eq!(code, 0, "second run must succeed, stderr:\n{second}");
    assert!(
        second.contains(TIER0_DECISION),
        "second run still carries the decision line, got:\n{second}"
    );
    assert!(
        !second.contains(TIER0_NOTICE),
        "second run within the hour must not repeat the notice, got:\n{second}"
    );

    // An hour having elapsed (simulated by backdating the state-file
    // timestamp) makes the notice due again — the window is an hour, not
    // "once ever".
    let stamp_path = world.state_dir().join("tier0-notice");
    let stamp: u64 = fs::read_to_string(&stamp_path)
        .expect("timestamp file exists")
        .trim()
        .parse()
        .expect("timestamp parses");
    fs::write(&stamp_path, (stamp - 3600).to_string()).expect("backdate timestamp");
    let (code, _out, third) = world.run_shim(false);
    assert_eq!(code, 0, "third run must succeed, stderr:\n{third}");
    assert!(
        third.contains(TIER0_NOTICE),
        "notice must return after the hour elapses, got:\n{third}"
    );
}

#[test]
fn no_config_failing_suite_is_recorded_faithfully() {
    let world = Tier0World::new("fail");

    let (exit_code, _stdout, stderr) = world.run_shim(true);

    // A failing suite is the suite's own failure, not an infra failure: the
    // exit code passes through faithfully (INV-3) and the verdict says so.
    assert_ne!(exit_code, 0, "failing fixture must exit non-zero");
    assert!(
        stderr.contains(TIER0_DECISION),
        "stderr must carry the Tier-0 decision line, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: TestFailure"),
        "stderr must classify the run as TestFailure, got:\n{stderr}"
    );

    let verdicts: Vec<_> = world
        .runlog_records()
        .into_iter()
        .filter(|r| r["rec"] == "verdict")
        .collect();
    assert_eq!(verdicts.len(), 1, "exactly one verdict record");
    assert_eq!(verdicts[0]["ran"], "local");
    assert_eq!(verdicts[0]["verdict"], "test_failure");
    assert_eq!(
        verdicts[0]["exit_code"], exit_code,
        "recorded exit is faithful"
    );
}

#[test]
fn non_intercepted_subcommand_still_takes_the_silent_passthrough() {
    let world = Tier0World::new("passthrough");

    // `cargo build` is not in the default intercept set: Tier-0 must not
    // turn the fast path into a logged run — passthrough stays silent and
    // leaves no runlog records behind.
    let mut cmd = Command::new(&world.cargo_link);
    cmd.current_dir(&world.project).args(["build"]);
    cmd.env("HOME", world.home());
    cmd.env("PATH", world.child_path());
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XDG_STATE_HOME");
    cmd.env_remove("GANTRY_LOCAL");
    cmd.env_remove("GANTRY_ON");
    // Same resolution determinism as run_shim: an inherited GANTRY_EXEC_PATH
    // would redirect even the passthrough arm's binary lookup.
    cmd.env_remove("GANTRY_EXEC_PATH");
    cmd.env_remove("CARGO_TARGET_DIR");
    cmd.env_remove("CARGO_BUILD_TARGET_DIR");
    let output = cmd.output().expect("run cargo shim build");

    assert_eq!(
        output.status.code(),
        Some(0),
        "passthrough build must succeed, stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("[gantry] decision:"),
        "passthrough must not print a decision line, got:\n{stderr}"
    );
    assert!(
        !stderr.contains(TIER0_NOTICE),
        "passthrough must not note the tier, got:\n{stderr}"
    );
    assert!(
        !world.state_dir().join("runs.jsonl").exists(),
        "passthrough must not write runlog records"
    );
}

// ============================================================================
// The cap wrapper (plan §"Tier-0": "pure local cap-wrapper") and
// `gantry quickcheck` (plan §"CLI surface": the Tier-0 proof), bf-139.
//
// The host's real systemd user manager may or may not exist, so every tier
// assertion here runs behind a fake `systemd-run` on the child's PATH prefix
// — the validating script proves the argv contract, the rejecting script
// proves the documented degrade — instead of asserting against whatever the
// host happens to provide.
// ============================================================================

/// The cap line the wrapper prints when the scope mechanism works, with the
/// Tier-0 default cap values (plan config model: 200% / 6G).
fn scoped_cap_note() -> String {
    "[gantry] cap: capping local runs via systemd-run scope (CPUQuota=200%, MemoryMax=6G)"
        .to_string()
}

/// The cap-tier line quickcheck prints when the scope mechanism works — the
/// same values as [`scoped_cap_note`], in quickcheck's `ok — <tier>` shape.
fn scoped_tier_line() -> String {
    "cap works: ok — systemd-run scope (CPUQuota=200%, MemoryMax=6G)".to_string()
}

#[test]
fn tier0_run_is_capped_through_a_systemd_scope_when_available() {
    let world = Tier0World::new("scoped");
    let fakes = TempDir::new().expect("fake bin dir");
    fake_systemd_run(fakes.path(), VALIDATING_EXEC);

    let (exit_code, _stdout, stderr) = world.run_shim_with_path_prefix(false, Some(fakes.path()));

    assert_eq!(exit_code, 0, "fixture must still pass, stderr:\n{stderr}");
    assert!(
        stderr.contains(&scoped_cap_note()),
        "scoped tier must be announced with the configured values, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the capped run must still produce its real verdict, got:\n{stderr}"
    );
}

#[test]
fn tier0_run_degrades_loudly_when_no_scope_is_available() {
    let world = Tier0World::new("degraded");
    let fakes = TempDir::new().expect("fake bin dir");
    fake_systemd_run(fakes.path(), REJECTING_EXEC);

    let (exit_code, _stdout, stderr) = world.run_shim_with_path_prefix(false, Some(fakes.path()));

    // The degrade is loud — it names the reason the scope was refused — but
    // it must never turn into a failed run: a broken gantry must not block
    // builds (plan §"config-failure policy" spirit; failure-modes table,
    // "systemd-run unavailable → plain exec").
    assert_eq!(
        exit_code, 0,
        "the degrade must not fail the run, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] cap: systemd-run scope unavailable (no user session)"),
        "degrade must be loud and name the reason, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the degraded run must still produce its real verdict, got:\n{stderr}"
    );
}

#[test]
fn quickcheck_passes_with_no_backend_configured() {
    // The bead's acceptance scenario verbatim: a fresh install with zero
    // config passes quickcheck with no backend. The scoped tier is pinned
    // with the validating fake so the assertion is deterministic.
    let world = Tier0World::new("quickcheck-ok");
    let fakes = TempDir::new().expect("fake bin dir");
    fake_systemd_run(fakes.path(), VALIDATING_EXEC);

    let (exit_code, stdout, _stderr) = world.run_quickcheck(Some(fakes.path()));

    assert_eq!(exit_code, 0, "quickcheck must pass, stdout:\n{stdout}");
    assert!(
        stdout.contains("no backend configured (Tier-0 zero-config mode)"),
        "header must name the tier, got:\n{stdout}"
    );
    assert!(
        stdout.contains("shim resolves: ok"),
        "shim check must pass, got:\n{stdout}"
    );
    assert!(
        stdout.contains(&scoped_tier_line()),
        "cap check must pass at the pinned scoped tier, got:\n{stdout}"
    );
    assert!(
        stdout.contains("git ok: ok") && stdout.contains("work tree with HEAD"),
        "git check must pass inside the fixture repo, got:\n{stdout}"
    );
    assert!(
        stdout.contains("passed ("),
        "summary must report the pass, got:\n{stdout}"
    );
}

#[test]
fn quickcheck_with_a_backend_configured_still_checks_the_tier0_surface() {
    // The header names the backend when one is configured — and the checks
    // still run and still pass, because quickcheck never contacts a backend
    // (INV-6): it is the no-backend sanity, useful as a baseline regardless
    // of what the config layers say. The user layer is written AFTER the
    // world's zero-config precondition assert, in its isolated HOME.
    let world = Tier0World::new("quickcheck-argo");
    let config_dir = world.home().join(".config/gantry");
    fs::create_dir_all(&config_dir).expect("create user config dir");
    fs::write(
        config_dir.join("config.toml"),
        "[remote]\nbackend = \"argo\"\n",
    )
    .expect("write user config with an argo backend");
    let fakes = TempDir::new().expect("fake bin dir");
    fake_systemd_run(fakes.path(), VALIDATING_EXEC);

    let (exit_code, stdout, _stderr) = world.run_quickcheck(Some(fakes.path()));

    assert_eq!(exit_code, 0, "quickcheck must pass, stdout:\n{stdout}");
    assert!(
        stdout.contains(
            "backend argo configured — these checks cover the no-backend (Tier-0) sanity only"
        ),
        "header must name the configured backend, got:\n{stdout}"
    );
    assert!(
        stdout.contains("shim resolves: ok")
            && stdout.contains(&scoped_tier_line())
            && stdout.contains("git ok: ok"),
        "all three checks must still be answered, got:\n{stdout}"
    );
}

#[test]
fn quickcheck_reports_the_documented_cap_degrade_instead_of_failing() {
    // Where scopes are impossible the degrade is the *active tier*, not a
    // failure (plan R5: report the tier; quickcheck must stay useful exactly
    // where gantry's plain-exec contract applies).
    let world = Tier0World::new("quickcheck-degraded");
    let fakes = TempDir::new().expect("fake bin dir");
    fake_systemd_run(fakes.path(), REJECTING_EXEC);

    let (exit_code, stdout, _stderr) = world.run_quickcheck(Some(fakes.path()));

    assert_eq!(
        exit_code, 0,
        "a capless environment must not fail quickcheck, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("cap works: ok — plain exec — no cgroup cap"),
        "the plain tier must be reported as the active tier, got:\n{stdout}"
    );
}

#[test]
fn quickcheck_fails_when_a_required_check_fails() {
    // A check that cannot fail proves nothing: with `cargo` removed from
    // resolution entirely (restricted PATH serving git and the fake
    // systemd-run but no cargo), the shim check must fail the run — exit
    // non-zero, the failed check named, the others still answered.
    let world = Tier0World::new("quickcheck-fail");
    let restricted = TempDir::new().expect("restricted bin dir");
    symlink_from_path(restricted.path(), "git");
    // `true` too: the cap probe execs it through the fake systemd-run, and
    // this test isolates the failure to the shim check alone.
    symlink_from_path(restricted.path(), "true");
    fake_systemd_run(restricted.path(), VALIDATING_EXEC);

    let (exit_code, stdout, _stderr) =
        world.run_quickcheck_with_path(&restricted.path().display().to_string());

    assert_ne!(exit_code, 0, "a failed check must fail quickcheck");
    assert!(
        stdout.contains("shim resolves: FAIL"),
        "the shim check must be the failure, got:\n{stdout}"
    );
    assert!(
        stdout.contains("cap works: ok") && stdout.contains("git ok: ok"),
        "the other checks must still be answered, got:\n{stdout}"
    );
    assert!(
        stdout.contains("FAILED ("),
        "summary must report the failure, got:\n{stdout}"
    );
}
