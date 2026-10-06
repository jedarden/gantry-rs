// gantry — doctor command for installation verification (plan Component 8).
//
// Phase 1a: doctor core checks (bf-2u0).
//
// This module defines:
// - PATH ordering check (shim-before-real)
// - Real binary resolution and self-recursion guard
// - Config parsing validation
// - Backend preflight (kubectl or command template)
// - systemd-run scope creation probe
// - Git identity check
// - Orphaned intent detection from runlog
// - E2E canary (doctor --e2e): one loopback round trip through the real
//   RefPusher → executor → verdict pipeline on a self-contained fixture
// - Overall health summary

use crate::backend::command::{CommandBackend, CommandConfig};
use crate::backend::{RemoteBackend, RunSpec, Verdict};
use crate::config::Config;
use crate::refs::RefPusher;
use crate::runlog::RunLog;
use crate::shim::{resolve_real_binary, shim_dir};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Result of a doctor check.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    /// Check passed.
    Pass,
    /// Check failed with a reason.
    Fail(String),
    /// Check skipped with a reason (e.g., optional feature not configured).
    Skipped(String),
    /// Check warning (doesn't fail overall but值得 noting).
    Warning(String),
}

/// Doctor check result with name.
#[derive(Debug)]
pub struct DoctorCheck {
    /// Check name (e.g., "PATH ordering").
    pub name: String,
    /// Check result.
    pub result: CheckResult,
}

impl DoctorCheck {
    /// Create a new passing check.
    pub fn pass(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            result: CheckResult::Pass,
        }
    }

    /// Create a new failing check.
    pub fn fail(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            result: CheckResult::Fail(reason.into()),
        }
    }

    /// Create a new skipped check.
    pub fn skipped(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            result: CheckResult::Skipped(reason.into()),
        }
    }

    /// Create a new warning check.
    pub fn warning(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            result: CheckResult::Warning(reason.into()),
        }
    }

    /// Check if this check passed.
    pub fn is_pass(&self) -> bool {
        matches!(self.result, CheckResult::Pass)
    }
}

/// Overall doctor health status.
#[derive(Debug)]
pub struct HealthStatus {
    /// All individual checks.
    pub checks: Vec<DoctorCheck>,
}

impl HealthStatus {
    /// Create a new health status.
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    /// Add a check to the status.
    pub fn add(&mut self, check: DoctorCheck) {
        self.checks.push(check);
    }

    /// Check if all critical checks passed.
    ///
    /// Warnings and skipped checks don't fail the overall health.
    /// Only fail results indicate unhealthy state.
    pub fn is_healthy(&self) -> bool {
        self.checks.iter().all(|c| {
            c.is_pass() || matches!(c.result, CheckResult::Warning(_) | CheckResult::Skipped(_))
        })
    }

    /// Get all failing checks.
    pub fn failures(&self) -> Vec<&DoctorCheck> {
        self.checks
            .iter()
            .filter(|c| matches!(c.result, CheckResult::Fail(_)))
            .collect()
    }

    /// Print the health status to stdout.
    pub fn print(&self) {
        println!("gantry doctor — system health checks");
        println!();

        for check in &self.checks {
            match &check.result {
                CheckResult::Pass => {
                    println!("✓ {}: OK", check.name);
                }
                CheckResult::Fail(reason) => {
                    println!("✗ {}: FAILED - {}", check.name, reason);
                }
                CheckResult::Skipped(reason) => {
                    println!("○ {}: SKIPPED - {}", check.name, reason);
                }
                CheckResult::Warning(reason) => {
                    println!("⚠ {}: WARNING - {}", check.name, reason);
                }
            }
        }

        println!();
        if self.is_healthy() {
            println!("Overall: HEALTHY");
        } else {
            println!("Overall: UNHEALTHY");
        }
    }
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Check PATH ordering: shim binary should appear before real cargo.
///
/// This ensures the gantry shim is actually being used when `cargo` is run.
fn check_path_ordering() -> DoctorCheck {
    let shim_dir_result = shim_dir();
    let shim_dir = match shim_dir_result {
        Ok(dir) => dir,
        Err(e) => {
            return DoctorCheck::fail(
                "PATH ordering",
                format!("Cannot find shim directory: {}", e),
            );
        }
    };

    // Get the PATH environment variable
    let path_var = match env::var("PATH") {
        Ok(p) => p,
        Err(_) => {
            return DoctorCheck::fail("PATH ordering", "PATH environment variable not set");
        }
    };

    // Check if shim dir is in PATH
    let shim_str = shim_dir.display().to_string();
    let path_entries: Vec<String> = env::split_paths(&path_var)
        .map(|p| p.to_str().map(|s| s.to_string()).unwrap_or_default())
        .collect();

    let shim_position = path_entries.iter().position(|p| p == &shim_str);

    if shim_position.is_none() {
        return DoctorCheck::fail(
            "PATH ordering",
            format!("Shim directory {} not in PATH", shim_dir.display()),
        );
    }

    // Check if cargo appears before shim in PATH
    let before_shim: Vec<String> = path_entries[..shim_position.unwrap()].to_vec();
    for dir in before_shim {
        let cargo_path = PathBuf::from(&dir).join("cargo");
        if cargo_path.exists() {
            return DoctorCheck::fail(
                "PATH ordering",
                format!(
                    "Real cargo found at {} appears before shim in PATH",
                    cargo_path.display()
                ),
            );
        }
    }

    DoctorCheck::pass("PATH ordering")
}

/// Check real binary resolution and self-recursion guard.
///
/// Ensures the shim can find the real cargo binary and won't exec itself.
fn check_real_binary() -> DoctorCheck {
    let config = match Config::hardcoded().try_get_real_binary() {
        Ok(Some(binary)) => binary,
        Ok(None) => {
            // No override configured, will use PATH search
            let cfg = Config::hardcoded();
            match resolve_real_binary(&cfg) {
                Ok(binary) => binary,
                Err(e) => {
                    return DoctorCheck::fail("Real binary resolution", e);
                }
            }
        }
        Err(e) => {
            return DoctorCheck::fail("Real binary resolution", e);
        }
    };

    // Verify the binary exists
    if !config.exists() {
        return DoctorCheck::fail(
            "Real binary resolution",
            format!("Resolved binary does not exist: {}", config.display()),
        );
    }

    // Check self-recursion guard: verify resolved binary is not the gantry shim
    let current_exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return DoctorCheck::warning(
                "Self-recursion guard",
                format!("Cannot determine current binary: {}", e),
            );
        }
    };

    // Canonicalize both paths for comparison
    let canonical_resolved = match config.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return DoctorCheck::warning(
                "Self-recursion guard",
                "Cannot canonicalize resolved binary",
            );
        }
    };

    let canonical_current = match current_exe.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return DoctorCheck::warning(
                "Self-recursion guard",
                "Cannot canonicalize current binary",
            );
        }
    };

    if canonical_resolved == canonical_current {
        return DoctorCheck::fail(
            "Self-recursion guard",
            format!(
                "Resolved binary is the gantry shim itself: {}",
                config.display()
            ),
        );
    }

    DoctorCheck::pass("Real binary resolution & self-recursion guard")
}

/// Check config parsing.
///
/// Validates that config file exists and parses correctly.
fn check_config_parse() -> DoctorCheck {
    let load_result = Config::load();

    // Report warnings but don't fail on them
    for warning in &load_result.warnings {
        eprintln!("Config warning: {}", warning);
    }

    if load_result.broken_banner.is_some() {
        return DoctorCheck::fail(
            "Config parsing",
            "Config is broken (check last-known-good banner)",
        );
    }

    DoctorCheck::pass("Config parsing")
}

/// Check backend preflight.
///
/// Validates that the configured backend is reachable and functional.
fn check_backend_preflight() -> DoctorCheck {
    let config = Config::load();

    match config.config.remote.backend {
        crate::config::Backend::None => {
            DoctorCheck::skipped("Backend preflight", "Tier-0 mode: no backend configured")
        }
        crate::config::Backend::Argo => {
            // Check if kubectl is available
            let kubectl_result = Command::new("kubectl")
                .args(["version", "--client", "--output=json"])
                .output();

            match kubectl_result {
                Ok(output) if output.status.success() => {
                    // Try to parse the JSON to verify it's valid
                    if serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(
                        &output.stdout,
                    ))
                    .is_ok()
                    {
                        DoctorCheck::pass("Backend preflight (kubectl)")
                    } else {
                        DoctorCheck::warning("Backend preflight", "kubectl output is invalid JSON")
                    }
                }
                Ok(_) => DoctorCheck::fail("Backend preflight", "kubectl --client failed"),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        DoctorCheck::fail("Backend preflight", "kubectl not found in PATH")
                    } else {
                        DoctorCheck::fail("Backend preflight", format!("kubectl failed: {}", e))
                    }
                }
            }
        }
        crate::config::Backend::Command => {
            // For command backend, check if the template is valid
            if config.config.remote.command.is_some() {
                DoctorCheck::pass("Backend preflight (command template)")
            } else {
                DoctorCheck::fail(
                    "Backend preflight",
                    "Command backend selected but no template configured",
                )
            }
        }
    }
}

/// Check systemd-run scope creation.
///
/// Probes whether systemd-run can create a scope (required for cgroup caps).
fn check_systemd_run() -> DoctorCheck {
    // Try to create a simple scope with a true command
    let result = Command::new("systemd-run")
        .args(["--scope", "--user", "-p", "CPUQuota=1%", "-q", "--", "true"])
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                DoctorCheck::pass("systemd-run scope creation")
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                DoctorCheck::warning(
                    "systemd-run scope creation",
                    format!("systemd-run failed: {}", stderr.trim()),
                )
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                DoctorCheck::warning(
                    "systemd-run scope creation",
                    "systemd-run not found (cgroup caps unavailable)",
                )
            } else {
                DoctorCheck::warning(
                    "systemd-run scope creation",
                    format!("systemd-run failed: {}", e),
                )
            }
        }
    }
}

/// Check git identity.
///
/// Verifies that git user.name and user.email are configured.
fn check_git_identity() -> DoctorCheck {
    let name_result = Command::new("git").args(["config", "user.name"]).output();
    let email_result = Command::new("git").args(["config", "user.email"]).output();

    match (name_result, email_result) {
        (Ok(name_output), Ok(email_output)) => {
            let name = String::from_utf8_lossy(&name_output.stdout)
                .trim()
                .to_string();
            let email = String::from_utf8_lossy(&email_output.stdout)
                .trim()
                .to_string();

            if name.is_empty() || email.is_empty() {
                DoctorCheck::fail(
                    "Git identity",
                    format!(
                        "Git identity incomplete: user.name='{}', user.email='{}'",
                        name, email
                    ),
                )
            } else {
                DoctorCheck::pass("Git identity")
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            DoctorCheck::fail("Git identity", format!("git config failed: {}", e))
        }
    }
}

/// Check for orphaned intent records in runlog.
///
/// Reports any OPEN intents without matching verdicts (INV-1).
fn check_orphaned_intents() -> DoctorCheck {
    let runlog = match RunLog::open() {
        Ok(rl) => rl,
        Err(e) => {
            return DoctorCheck::warning("Orphaned intents", format!("Cannot open runlog: {}", e))
        }
    };

    match runlog.find_orphans() {
        Ok(orphans) => {
            if orphans.is_empty() {
                DoctorCheck::pass("Orphaned intents")
            } else {
                DoctorCheck::warning(
                    "Orphaned intents",
                    format!(
                        "Found {} orphaned intent(s) (possible lost runs)",
                        orphans.len()
                    ),
                )
            }
        }
        Err(e) => DoctorCheck::warning("Orphaned intents", format!("Cannot check orphans: {}", e)),
    }
}

/// Run all doctor checks and return overall health status.
///
/// This is the main entry point for `gantry doctor`.
pub fn run_all_checks() -> HealthStatus {
    let mut health = HealthStatus::new();

    // Run all core checks in order
    health.add(check_path_ordering());
    health.add(check_real_binary());
    health.add(check_config_parse());
    health.add(check_backend_preflight());
    health.add(check_systemd_run());
    health.add(check_git_identity());
    health.add(check_orphaned_intents());

    health
}

// ============================================================================
// E2E canary — `gantry doctor --e2e` (plan §8)
// ============================================================================

/// The reference executor (contrib/gantry-exec.sh), compiled into the binary.
///
/// `gantry doctor --e2e` must run on an installed box where no source checkout
/// — and no `./contrib` — exists (plan DD-6, single static binary). This is the
/// same file the shipped command-template defaults invoke; embedding it at
/// build time keeps one source of truth and makes the canary self-contained.
/// Shared with [`crate::cli::init`], which installs this same copy on an
/// onboarding target — the script the preset then invokes is the script the
/// canary ran.
pub(crate) const E2E_EXECUTOR_SCRIPT: &str = include_str!("../contrib/gantry-exec.sh");

/// The canary fixture's Cargo.toml: a minimal crate with zero dependencies, so
/// the executor's `cargo test` needs no crates.io access and no warm cache.
const CANARY_CARGO_TOML: &str = "\
[package]
name = \"gantry-e2e-canary\"
version = \"0.1.0\"
edition = \"2021\"
";

/// The canary fixture's single known-good test — the content the round trip
/// ships. A suite that actually compiles and runs (rather than a help flag)
/// is the smallest honest proof that push → clone → build → verdict works.
const CANARY_LIB_RS: &str = "\
//! `gantry doctor --e2e` canary fixture: a minimal crate with one
//! known-good test. The canary pushes this content through the real
//! RefPusher and backend and expects a Pass verdict back.

#[cfg(test)]
mod canary {
    #[test]
    fn round_trip_reaches_the_toolchain() {
        assert_eq!(2 + 2, 4);
    }
}
";

/// Tunables for the e2e canary round trip.
///
/// Production callers use [`E2eCanaryOptions::default()`]; the field exists so
/// integration tests can aim a known-bad component at a specific leg and
/// assert the failure names that leg.
#[derive(Debug, Clone, Default)]
pub struct E2eCanaryOptions {
    /// The cargo the loopback executor runs. `None` resolves the real
    /// toolchain by the shim rules ([`resolve_real_binary`]) — honoring any
    /// configured `real_binary` override — so the executor never recurses
    /// into a gantry shim; a resolution failure degrades to the executor's
    /// own PATH default ("cargo").
    pub exec_cargo: Option<PathBuf>,
}

/// Run end-to-end canary test (doctor --e2e).
///
/// One full gantry round trip through the loopback backend (plan §8): a tiny
/// known-good fixture commit is pushed through the real [`RefPusher`] to a
/// throwaway bare remote, the embedded reference executor clones that epoch
/// ref and runs the fixture's `cargo test`, and the command-backend wait maps
/// the result onto the verdict ladder — push → clone → contract → verdict,
/// exactly the pipeline a real offloaded run takes, with no external service
/// involved. The fixture lives under the system temp dir and is removed when
/// the canary finishes.
///
/// `Err` names the leg that failed (`fixture`, `push`, `submit`, `wait`, or
/// `verdict`) with an actionable message; `gantry doctor --e2e` exits non-zero
/// with that message (plan §8: "one command answers 'is the pipeline actually
/// working'").
pub fn run_e2e_test() -> Result<String, String> {
    let config = Config::load().config;
    run_e2e_canary(&config, &E2eCanaryOptions::default())
}

/// [`run_e2e_test`] against an explicit config and options.
///
/// The canary always rides the loopback backend — the embedded reference
/// executor against the fixture's own bare remote — regardless of the
/// configured backend: an argo-configured install would otherwise turn a
/// doctor check into a cluster submission, and the canary's question ("is the
/// gantry pipeline itself working?") is answerable entirely on this box. The
/// config still steers what it truthfully can: the fixture remote is added
/// under the configured `ci_remote` name, the epoch ref follows the
/// configured `push_mode`, and the wait deadline is [`Config::command_deadline`].
pub fn run_e2e_canary(config: &Config, opts: &E2eCanaryOptions) -> Result<String, String> {
    let started = Instant::now();

    // Leg: fixture — loopback remote, known-good commit, executor preset.
    let fixture = build_canary_fixture(config, opts).map_err(|e| {
        e2e_failure(
            "fixture",
            &format!(
                "{e} — the canary builds a throwaway git repo and executor \
                 under {}; disk space and temp-dir permissions are the usual causes",
                std::env::temp_dir().display()
            ),
        )
    })?;

    let outcome = drive_canary_round_trip(config, &fixture, started);
    // The fixture is throwaway by construction; a failed round trip must not
    // leave it behind either. Best effort — a stranded temp dir is cosmetic.
    let _ = fs::remove_dir_all(&fixture.root);
    outcome
}

/// The built canary fixture: everything the round-trip legs need, under one
/// throwaway root.
struct CanaryFixture {
    /// Temp root — removed when the canary finishes.
    root: PathBuf,
    /// Work repo holding the known-good commit (the RefPusher runs here).
    repo_dir: PathBuf,
    /// The bare loopback remote, as a URL the executor can fetch.
    remote_url: String,
    /// The known-good commit the round trip ships.
    sha: String,
    /// The command-template backend wired to the fixture's executor wrapper.
    backend: CommandBackend,
}

/// Leg "fixture": build the loopback world — bare remote, known-good cargo
/// fixture commit, and the embedded executor with a wrapper pinning its
/// private state dir and cargo binary.
fn build_canary_fixture(config: &Config, opts: &E2eCanaryOptions) -> Result<CanaryFixture, String> {
    let root = std::env::temp_dir().join(format!(
        "gantry-e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;

    // Loopback remote: a bare repo the RefPusher pushes epoch refs to and the
    // executor fetches them from — the real git transport, no service.
    let remote_dir = root.join("remote.git");
    git_in(
        &root,
        &[
            "init",
            "--bare",
            "--quiet",
            &remote_dir.display().to_string(),
        ],
    )?;
    let remote_url = format!("file://{}", remote_dir.display());

    // Work repo with the known-good fixture commit.
    let repo_dir = root.join("repo");
    git_in(&root, &["init", "--quiet", &repo_dir.display().to_string()])?;
    git_in(&repo_dir, &["config", "user.name", "gantry doctor --e2e"])?;
    git_in(&repo_dir, &["config", "user.email", "gantry-e2e@localhost"])?;
    fs::write(repo_dir.join("Cargo.toml"), CANARY_CARGO_TOML)
        .map_err(|e| format!("cannot write fixture Cargo.toml: {e}"))?;
    fs::create_dir_all(repo_dir.join("src"))
        .map_err(|e| format!("cannot write fixture src/: {e}"))?;
    fs::write(repo_dir.join("src/lib.rs"), CANARY_LIB_RS)
        .map_err(|e| format!("cannot write fixture src/lib.rs: {e}"))?;
    git_in(&repo_dir, &["add", "."])?;
    git_in(
        &repo_dir,
        &[
            "commit",
            "--quiet",
            "-m",
            "gantry doctor --e2e canary fixture",
        ],
    )?;
    let sha = git_in(&repo_dir, &["rev-parse", "HEAD"])?;
    // Under the configured remote name, so the canary exercises the same
    // push target name a real run uses.
    git_in(
        &repo_dir,
        &["remote", "add", &config.remote.ci_remote, &remote_url],
    )?;

    // The reference executor, materialized from the embedded copy, plus a
    // wrapper that pins the two knobs the canary owns: a private run-state
    // dir (never the shared /tmp/gantry-runs default) and the cargo binary
    // (shim-resolved, so a PATH-shimmed box does not recurse).
    let executor = root.join("gantry-exec.sh");
    fs::write(&executor, E2E_EXECUTOR_SCRIPT)
        .map_err(|e| format!("cannot write executor script: {e}"))?;
    make_executable(&executor)?;
    let exec_cargo = match &opts.exec_cargo {
        Some(path) => path.clone(),
        None => resolve_real_binary(config).unwrap_or_else(|_| PathBuf::from("cargo")),
    };
    let state_dir = root.join("executor-state");
    fs::create_dir_all(&state_dir).map_err(|e| format!("cannot create executor state dir: {e}"))?;
    let wrapper = root.join("gantry-exec-canary.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/env sh\n\
             # Generated by `gantry doctor --e2e`: pins the loopback canary's\n\
             # executor state dir and cargo so the shipped executor needs no\n\
             # inherited environment.\n\
             GANTRY_EXEC_STATE_DIR={} GANTRY_EXEC_CARGO={} exec {} \"$@\"\n",
            sh_quote(&state_dir.to_string_lossy()),
            sh_quote(&exec_cargo.to_string_lossy()),
            sh_quote(&executor.to_string_lossy()),
        ),
    )
    .map_err(|e| format!("cannot write executor wrapper: {e}"))?;
    make_executable(&wrapper)?;
    let wrapper_str = wrapper.display().to_string();

    let backend = CommandBackend::with_config(CommandConfig {
        submit: vec![
            wrapper_str.clone(),
            "submit".to_string(),
            "{repo}".to_string(),
            "{rev}".to_string(),
            "{args_json}".to_string(),
        ],
        logs: vec![
            wrapper_str.clone(),
            "logs".to_string(),
            "{handle}".to_string(),
        ],
        wait: vec![wrapper_str, "wait".to_string(), "{handle}".to_string()],
        status: None,
    });

    Ok(CanaryFixture {
        root,
        repo_dir,
        remote_url,
        sha,
        backend,
    })
}

/// Legs "push" → "submit" → "wait" → "verdict": the round trip itself.
fn drive_canary_round_trip(
    config: &Config,
    fixture: &CanaryFixture,
    started: Instant,
) -> Result<String, String> {
    // Leg: push — the real RefPusher, from the fixture repo.
    let run_id = format!(
        "doctor-e2e-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let push = RefPusher::push_in_repo(config, &fixture.repo_dir, &fixture.sha, &run_id);
    if !push.success {
        return Err(e2e_failure(
            "push",
            &format!(
                "could not push {} to the loopback remote: {} \
                 — the canary pushes to a local bare repo, so a failure here \
                 means git itself is broken",
                push.ref_name, push.reason
            ),
        ));
    }

    // Leg: submit — the command-template backend path, trivial argv.
    let spec = RunSpec::new(
        "cargo",
        "test",
        Vec::new(),
        &fixture.remote_url,
        &fixture.sha,
        "",
    );
    let handle = fixture.backend.submit(&spec).map_err(|e| {
        e2e_failure(
            "submit",
            &format!(
                "the loopback backend's submit command failed: {} \
                 — the command-template presets are broken; check [remote.command] \
                 or reinstall gantry",
                e.reason
            ),
        )
    })?;

    // Leg: wait — the backend's wait command, deadline-bounded.
    let deadline = Instant::now() + config.command_deadline();
    match fixture.backend.wait(&handle, deadline) {
        Ok(Verdict::Pass) => Ok(format!(
            "round trip passed (push → clone → cargo test → verdict) in {:?}",
            started.elapsed()
        )),
        Ok(verdict) => Err(e2e_failure_with_logs(
            fixture,
            &handle,
            "verdict",
            &format!(
                "the known-good canary suite came back {verdict}, expected Pass \
                 — the remote contract or the toolchain misclassified it"
            ),
        )),
        Err(e) if e.deadline_exceeded => Err(e2e_failure_with_logs(
            fixture,
            &handle,
            "wait",
            &format!("deadline exceeded before a verdict: {}", e.reason),
        )),
        Err(e) => Err(e2e_failure_with_logs(
            fixture,
            &handle,
            "wait",
            &format!("wait command failed: {}", e.reason),
        )),
    }
}

/// Format an e2e failure: the leg is always named first, so a non-zero exit
/// points the operator at the exact stage that broke (plan §8).
fn e2e_failure(leg: &str, detail: &str) -> String {
    format!("e2e canary failed at leg '{leg}': {detail}")
}

/// [`e2e_failure`], plus a best-effort tail of the executor's captured log —
/// the fastest route from "verdict leg failed" to the stderr line that
/// explains it.
fn e2e_failure_with_logs(
    fixture: &CanaryFixture,
    handle: &crate::backend::RunHandle,
    leg: &str,
    detail: &str,
) -> String {
    let mut message = e2e_failure(leg, detail);
    let mut logs = Vec::new();
    if fixture.backend.stream_logs(handle, &mut logs).is_ok() && !logs.is_empty() {
        let text = String::from_utf8_lossy(&logs);
        let lines: Vec<&str> = text.lines().collect();
        let tail_start = lines.len().saturating_sub(10);
        let tail = lines[tail_start..].join("\n");
        if !tail.is_empty() {
            message.push_str("; executor log tail:\n");
            message.push_str(&tail);
        }
    }
    message
}

/// Run `git <args>` in `dir` and return trimmed stdout; any failure carries
/// stderr so the failing leg's message is actionable.
fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Make a script executable (no-op off Unix — the spawn failure surfaces at
/// the submit leg there, which is this unix-first tool's honest answer).
fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm)
            .map_err(|e| format!("cannot chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Single-quote text for safe inclusion in a POSIX shell command line: the
/// result is one shell word whose content survives byte-exact (`'` becomes
/// the close-quote/backslash-quote/open-quote idiom). Shared with
/// `cli::init`, which builds the ssh forwarder script the same way.
pub(crate) fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Run fault-injection fire drill (doctor --drill).
///
/// Injects a synthetic InfraFailure and asserts the entire loud-degrade chain fires.
pub fn run_drill() -> Result<String, String> {
    // TODO: Implement fire drill
    // This requires:
    // 1. Inject synthetic failure via drill-scoped hook
    // 2. Assert banner prints
    // 3. Assert intent/verdict records written
    // 4. Assert semaphore-gated capped local run
    // 5. Assert correct exit code

    Err("Fire drill not yet implemented".to_string())
}

/// Unit tests for the e2e canary's pure helpers. The round trip itself runs
/// real git and real cargo — that is integration-test territory
/// (tests/doctor_e2e_integration.rs).
#[cfg(test)]
mod e2e_canary_tests {
    use super::*;

    #[test]
    fn failure_message_names_the_leg_first() {
        let msg = e2e_failure("push", "no such remote");
        assert!(
            msg.contains("leg 'push'"),
            "leg must be named for the operator, got: {msg}"
        );
        assert!(msg.contains("no such remote"), "got: {msg}");
    }

    /// Failing path, push leg: with the fixture's loopback remote gone, the
    /// real RefPusher cannot push, and the round trip must fail naming the
    /// push leg — infrastructure broke before any suite ran (plan §8).
    ///
    /// Single-leg by construction: the failure fires before submit, so no
    /// cargo is ever invoked — the reason this lives in the unit module
    /// rather than the integration file, which owns the full round trip.
    #[test]
    fn e2e_push_failure_names_the_push_leg() {
        let config = Config::hardcoded();
        let fixture = build_canary_fixture(&config, &E2eCanaryOptions::default())
            .expect("the canary fixture builds wherever git works");
        git_in(
            &fixture.repo_dir,
            &["remote", "remove", &config.remote.ci_remote],
        )
        .expect("removing the fixture's own remote must succeed");
        let err = drive_canary_round_trip(&config, &fixture, Instant::now())
            .expect_err("a push without a remote must fail the round trip");
        assert!(
            err.contains("leg 'push'"),
            "the error must name the push leg, got: {err}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    /// Failing path, submit leg: a wrapper whose submit command exits
    /// non-zero fails the backend submission, and the round trip must fail
    /// naming the submit leg — the command-template presets, not the suite,
    /// broke (plan §8). The push leg is intact here, so this exercises the
    /// first backend leg in isolation.
    #[test]
    fn e2e_submit_failure_names_the_submit_leg() {
        let config = Config::hardcoded();
        let fixture = build_canary_fixture(&config, &E2eCanaryOptions::default())
            .expect("the canary fixture builds wherever git works");
        // A wrapper that fails every subcommand: submit never yields a
        // handle, so the round trip must stop at the submit leg.
        let wrapper = fixture.root.join("gantry-exec-canary.sh");
        fs::write(&wrapper, "#!/usr/bin/env sh\nexit 7\n")
            .expect("overwriting the fixture's own wrapper must succeed");
        make_executable(&wrapper).expect("re-chmodding the wrapper must succeed");
        let err = drive_canary_round_trip(&config, &fixture, Instant::now())
            .expect_err("a submit command that exits 7 must fail the round trip");
        assert!(
            err.contains("leg 'submit'"),
            "the error must name the submit leg, got: {err}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    /// Failing path, wait leg: the wait command is spawned, not
    /// exit-code-mapped — a non-zero wait *exit* would map through the
    /// verdict ladder onto the verdict leg, so the only fast route to this
    /// leg is a wait command that cannot even start. The wrapper therefore
    /// removes itself during submit: push and submit complete, and the wait
    /// spawn fails with command-not-found, which must fail the round trip
    /// naming the wait leg (plan §8).
    ///
    /// Single-leg by construction: submit emits its handle before the wait
    /// argv is ever resolved, so no suite, toolchain, or deadline is
    /// involved — the reason this lives in the unit module rather than the
    /// integration file, which owns the full round trip.
    #[test]
    fn e2e_wait_failure_names_the_wait_leg() {
        let config = Config::hardcoded();
        let fixture = build_canary_fixture(&config, &E2eCanaryOptions::default())
            .expect("the canary fixture builds wherever git works");
        // A wrapper whose submit leg removes the script (a running script
        // may unlink itself) and then emits a handle, so the backend's wait
        // spawn cannot find the command — the wait leg's error path without
        // any toolchain or deadline involved.
        let wrapper = fixture.root.join("gantry-exec-canary.sh");
        fs::write(
            &wrapper,
            "#!/usr/bin/env sh\ncase \"$1\" in\n  submit) rm -f \"$0\"; echo doctor-e2e-wait-leg-handle ;;\n  *) exit 0 ;;\nesac\n",
        )
        .expect("overwriting the fixture's own wrapper must succeed");
        make_executable(&wrapper).expect("re-chmodding the wrapper must succeed");
        let err = drive_canary_round_trip(&config, &fixture, Instant::now())
            .expect_err("a wait command that cannot spawn must fail the round trip");
        assert!(
            err.contains("leg 'wait'"),
            "the error must name the wait leg, got: {err}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn sh_quote_survives_hostile_paths() {
        let quoted = sh_quote("/tmp/it's a test");
        assert_eq!(quoted, "'/tmp/it'\\''s a test'");
    }

    #[test]
    fn embedded_executor_is_the_reference_script() {
        // include_str! makes a missing file a build error; this guards the
        // subtler failure of an empty or wrong-file embed.
        assert!(E2E_EXECUTOR_SCRIPT.starts_with("#!"));
        assert!(E2E_EXECUTOR_SCRIPT.contains("cmd_submit"));
        assert!(E2E_EXECUTOR_SCRIPT.contains("cmd_wait"));
    }

    #[test]
    fn canary_fixture_is_dependency_free_and_known_good() {
        // Zero dependencies keeps the executor's cargo test offline-capable.
        assert!(
            !CANARY_CARGO_TOML.contains("dependencies"),
            "the canary fixture must not need crates.io"
        );
        assert!(CANARY_LIB_RS.contains("#[test]"));
    }

    #[test]
    fn default_options_resolve_exec_cargo_at_call_time() {
        let opts = E2eCanaryOptions::default();
        assert!(opts.exec_cargo.is_none());
    }
}
