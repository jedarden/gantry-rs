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
// - Overall health summary

use crate::backend::command::CommandBackend;
use crate::backend::{RemoteBackend, RunSpec};
use crate::config::Config;
use crate::refs::RefPusher;
use crate::runlog::{Decision as LedgerDecision, RanLocation, RunEntry, RunLog};
use crate::shim::{resolve_real_binary, shim_dir};
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use crate::backend::Verdict as BackendVerdict;
use crate::drill;

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

/// Run end-to-end canary test (doctor --e2e).
///
/// Pushes the current repo's HEAD as a normal gantry ref and submits it to
/// the configured backend with the argv overridden to a trivial command, then
/// asserts the Pass verdict round-trips: push → clone → contract → verdict
/// without running a real suite (plan Component 8). One command answers "is
/// the pipeline actually working" on the operator's schedule, and
/// `gantry init --ssh` runs it as its final step; the round-trip split is in
/// the report (plan R3: "`doctor --e2e` measures round-trip").
///
/// The trivial argv composes with the shipped executor's contract:
/// `contrib/gantry-exec.sh` fixes the subcommand (`cargo test "$@"`), so the
/// canary's tail args are `["--help"]` — the backend runs `cargo test
/// --help`, which exits 0 without compiling anything. (`cargo --version`, the
/// plan's example, is not expressible through that contract — `cargo test
/// --version` is a cargo usage error — and `cargo test` bare would run the
/// operator's real suite, exactly what a canary must not do.)
///
/// Deliberately NOT the decision pipeline's degrade path: the canary must
/// fail when the pipeline is broken, and [`crate::decision::run_explicit`]'s
/// admission-semaphore ladder would mask exactly that — a dead backend
/// degrades to a capped local run whose trivial command exits 0. So the
/// canary drives the real stages directly ([`RefPusher::push`] → the real
/// backend's submit/wait) and reports the first broken stage instead.
pub fn run_e2e_test() -> Result<String, String> {
    let config = Config::load().config;
    if config.remote.backend == crate::config::Backend::None {
        return Err(
            "no remote backend configured (Tier-0) — the canary round-trips a real \
             backend, and there is nothing to round-trip; configure one (e.g. \
             `gantry init --ssh user@host`)"
                .to_string(),
        );
    }

    // The canary runs the operator's current repo: HEAD is the content the
    // backend will fetch and check out.
    if !crate::gate::is_inside_work_tree().unwrap_or(false) {
        return Err(
            "not inside a git work tree — run `gantry doctor --e2e` from the \
             repository whose pipeline you want to canary"
                .to_string(),
        );
    }
    let sha = crate::gate::head_sha()?;
    let repo_url = e2e_repo_url(&config);
    let run_id = format!(
        "e2e-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    eprintln!(
        "[gantry] e2e: canary {run_id}: pushing {sha} to remote `{}`",
        config.remote.ci_remote
    );
    let push_start = Instant::now();
    let pushed = RefPusher::push(&config, &sha, &run_id);
    let push_ms = push_start.elapsed().as_millis();
    if !pushed.success {
        // S-5: a git push failure can echo the remote URL, credentials and
        // all — the same redaction the crash bundle applies to raw responses.
        let reason = crate::crash::redact_url_userinfo(&pushed.reason);
        return Err(format!(
            "push failed after {push_ms}ms: {reason} (the backend never saw the run)"
        ));
    }
    eprintln!("[gantry] e2e: pushed {}", pushed.ref_name);

    // The same backend construction the decision pipeline resolves (user
    // command templates, or the shipped defaults) — the canary is only
    // honest if it drives the backend a real run would drive.
    let backend = match &config.remote.command {
        Some(c) => CommandBackend::with_config(crate::backend::command::CommandConfig {
            submit: c.submit.clone(),
            logs: c.logs.clone(),
            wait: c.wait.clone(),
            status: None, // the config schema carries no status step
        }),
        None => CommandBackend::new(),
    };

    let spec = RunSpec::new(
        "cargo",
        "test",
        vec!["--help".to_string()],
        &repo_url,
        &sha,
        "",
    );
    let handle = backend.submit(&spec).map_err(|e| {
        format!(
            "submit failed: {} (the push succeeded, so the backend or its \
                 command templates are broken)",
            e.reason
        )
    })?;
    eprintln!(
        "[gantry] e2e: submitted {}, waiting for the verdict",
        handle.handle
    );

    let wait_start = Instant::now();
    let deadline = Instant::now() + crate::decision::backend_wait_deadline(&config);
    let waited = backend.wait(&handle, deadline);
    let wait_ms = wait_start.elapsed().as_millis();

    match waited {
        Ok(BackendVerdict::Pass) => Ok(format!(
            "pipeline round trip OK: push {push_ms}ms + backend {wait_ms}ms → \
             verdict Pass (ref {}, run {run_id})",
            pushed.ref_name
        )),
        Ok(other) => Err(format!(
            "backend returned verdict {other} after {wait_ms}ms — expected Pass \
             for the trivial canary argv (run {run_id}, handle {})",
            handle.handle
        )),
        Err(e) => Err(format!(
            "wait failed after {wait_ms}ms: {} (run {run_id}, handle {})",
            e.reason, handle.handle
        )),
    }
}

/// The repo URL the canary submits, resolved the way the dispatcher resolves
/// it: the ci_remote's URL, with local paths made `file://`. The dispatcher's
/// copy (`get_repo_url` in main.rs) is binary-private, so the rule is spelled
/// here once more — the two must agree or the canary submits a URL a real
/// run would never use.
fn e2e_repo_url(config: &Config) -> String {
    Command::new("git")
        .args(["remote", "get-url", &config.remote.ci_remote])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| normalize_repo_url(String::from_utf8_lossy(&out.stdout).trim()))
        .unwrap_or_else(|| {
            format!(
                "file://{}",
                env::current_dir()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            )
        })
}

/// The dispatcher's repo-URL rule: a local path (absolute, or relative) is
/// submitted as a `file://` URL; everything else passes through unchanged.
fn normalize_repo_url(url: &str) -> String {
    if url.starts_with('/') || url.starts_with('.') {
        format!("file://{url}")
    } else {
        url.to_string()
    }
}

/// Run fault-injection fire drill (doctor --drill).
///
/// Injects a synthetic InfraFailure through the drill-scoped hook and asserts
/// the entire loud-degrade chain fires (plan Component 8): the timeout
/// banner naming the drill run, the drill-named infra reason, the write-ahead
/// intent and terminal verdict records, the admission-semaphore capped local
/// run, and an exit code faithful to that local run.
///
/// Mechanism: this process snapshots the run ledger, spawns itself as
/// [`drill::DRILL_RUN_SUBCOMMAND`] — which is what arms the hook, so nothing
/// outside a drill process can ever arm it — and asserts the chain from the
/// child's stderr and the ledger delta. The child's exit code IS the
/// pipeline's exit code, which is what makes the faithfulness link
/// assertable. The child's captured stderr is echoed through untouched: the
/// chain itself is the evidence, not a summary of it.
pub fn run_drill() -> Result<String, String> {
    // Pre-child ledger snapshot: the drill run's records are the delta, so
    // the assertion cannot be fooled by anything already in the ledger.
    let before: HashSet<String> = match RunLog::open().and_then(|rl| rl.read_entries()) {
        Ok(ledger) => ledger
            .entries
            .iter()
            .map(|e| e.intent.run_id.clone())
            .collect(),
        Err(e) => {
            return Err(format!(
                "cannot read the run ledger ({e}) — the drill asserts its \
                 intent/verdict records, so the ledger must be readable"
            ))
        }
    };

    let started = Instant::now();
    let exe = env::current_exe().map_err(|e| format!("cannot locate the gantry binary: {e}"))?;
    let output = Command::new(exe)
        .arg(drill::DRILL_RUN_SUBCOMMAND)
        .output()
        .map_err(|e| format!("cannot spawn the drill run: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit = output.status.code().unwrap_or(-1);

    // The child's chain is the evidence — pass it through so the operator
    // sees the real degrade, not a summary of it.
    eprint!("{stderr}");

    let drill_entries: Vec<RunEntry> = match RunLog::open().and_then(|rl| rl.read_entries()) {
        Ok(ledger) => ledger
            .entries
            .into_iter()
            .filter(|e| !before.contains(&e.intent.run_id))
            .collect(),
        Err(e) => {
            return Err(format!(
                "cannot re-read the run ledger after the drill ({e})"
            ))
        }
    };

    let links = drill_links(&stderr, exit, &drill_entries);
    for (name, fired) in &links {
        println!("  {} {name}", if *fired { "✓" } else { "✗" });
    }

    let missed: Vec<&str> = links.iter().filter(|(_, f)| !*f).map(|(n, _)| *n).collect();
    if missed.is_empty() {
        Ok(format!(
            "full degrade chain fired in {}ms: timeout banner, drill-named \
             infra reason, write-ahead intent + terminal verdict records, \
             semaphore-gated capped local run, faithful exit {exit}",
            started.elapsed().as_millis()
        ))
    } else {
        Err(format!(
            "the degrade chain did not fully fire — {} link(s) missing: {}. \
             The synthetic failure must reach the backend wait boundary: run \
             `gantry doctor --drill` from a clean git work tree whose `{}` \
             remote exists; `gantry report` shows what the pipeline recorded.",
            missed.len(),
            missed.join("; "),
            Config::load().config.remote.ci_remote,
        ))
    }
}

/// The fire drill's link-by-link assertion, pure so the whole chain contract
/// is unit-testable without spawning the drill process: did the synthetic
/// failure produce the banner, the records, the capped run, and a faithful
/// exit code?
///
/// `stderr` is the drill child's captured stderr; `child_exit` its process
/// exit code (−1 when it died by signal); `drill_entries` the ledger entries
/// the drill run added (the delta against the pre-run snapshot — nothing
/// already in the ledger can satisfy a link).
fn drill_links(
    stderr: &str,
    child_exit: i32,
    drill_entries: &[RunEntry],
) -> Vec<(&'static str, bool)> {
    let submitted_needle = format!("[gantry] submitted: {}", drill::DRILL_HANDLE);
    let banner_needle = format!(
        "[gantry] timeout: run {} exceeded its deadline",
        drill::DRILL_HANDLE
    );

    // The backend was the drill's synthetic one (the fixed handle proves the
    // injection actually fired rather than a real submission slipping by).
    let submitted = stderr.contains(submitted_needle.as_str());
    // The loud banner: a deadline expiry reads differently from every other
    // wait failure (plan §"failure modes"), and the drill must prove that
    // line specifically.
    let banner = stderr.contains(banner_needle.as_str());
    // The degrade must name itself as a drill — never mistakable in a
    // transcript or post-mortem for a real outage.
    let infra_named = stderr.contains(drill::DRILL_INFRA_REASON);
    // The ladder: the capped local run the caller's result actually came
    // from ([`crate::local::run_fallback_program`]'s entry lines).
    let ladder = stderr.contains("[gantry] falling back to capped local run");

    // The records: the write-ahead intent decided remote, and a terminal
    // verdict closed the run as `ran: local_after_infra` (INV-1 pairing).
    let intent_remote = drill_entries
        .iter()
        .any(|e| matches!(e.intent.decision, LedgerDecision::Remote));
    let capped_verdict = drill_entries
        .iter()
        .find_map(|e| e.verdict.as_ref())
        .filter(|v| matches!(v.ran, RanLocation::LocalAfterInfra));
    let verdict_recorded = capped_verdict.is_some();
    // The faithful exit (INV-3): the process exit code is the capped local
    // run's own, and for the drill's trivial argv (`cargo --version`) that
    // run passes — anything else means the chain lied about the result.
    let exit_faithful = capped_verdict
        .map(|v| v.exit_code == child_exit && v.verdict == crate::runlog::Verdict::Pass)
        .unwrap_or(false);

    vec![
        ("the drill backend took the submission", submitted),
        ("the synthetic failure raised the timeout banner", banner),
        (
            "the degrade named the drill as its infra reason",
            infra_named,
        ),
        (
            "the semaphore-gated ladder fell back to a capped local run",
            ladder,
        ),
        (
            "the write-ahead intent recorded decision: remote",
            intent_remote,
        ),
        (
            "a terminal verdict recorded ran: local_after_infra",
            verdict_recorded,
        ),
        (
            "the exit code is the capped local run's own (Pass)",
            exit_faithful,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    mod repo_url_rule {
        use super::*;

        #[test]
        fn local_paths_become_file_urls() {
            assert_eq!(normalize_repo_url("/srv/git/repo"), "file:///srv/git/repo");
            assert_eq!(
                normalize_repo_url("../relative/repo"),
                "file://../relative/repo"
            );
        }

        #[test]
        fn remote_urls_pass_through_unchanged() {
            assert_eq!(
                normalize_repo_url("https://git.example.com/x/y.git"),
                "https://git.example.com/x/y.git"
            );
            assert_eq!(
                normalize_repo_url("git@host:team/repo.git"),
                "git@host:team/repo.git"
            );
        }
    }

    mod fire_drill_links {
        use super::*;
        use crate::runlog::{Decision, GateInputs, IntentRecord, Verdict, VerdictRecord};
        use std::collections::HashMap;
        use std::path::PathBuf;

        /// The stderr the real chain prints, byte for byte from the
        /// pipeline's own format strings.
        fn chain_stderr() -> String {
            format!(
                "[gantry] decision: remote execution eligible\n\
                 [gantry] submitted: {}\n\
                 [gantry] timeout: run {} exceeded its deadline before a verdict was returned\n\
                 [gantry] infra: wait failed: {}\n\
                 [gantry] falling back to capped local run\n\
                 [gantry] verdict: Pass\n",
                drill::DRILL_HANDLE,
                drill::DRILL_HANDLE,
                drill::DRILL_INFRA_REASON,
            )
        }

        /// One ledger entry shaped exactly like the drill run's: a remote
        /// decision intent closed by a `local_after_infra` verdict.
        fn drill_entry(exit_code: i32, verdict: Verdict, ran: RanLocation) -> RunEntry {
            let intent = IntentRecord::new(
                "cargo".to_string(),
                vec!["--version".to_string()],
                "file:///repo".to_string(),
                "abc123".to_string(),
                PathBuf::from("."),
                GateInputs {
                    worktree: true,
                    head: true,
                    remote: true,
                    clean: true,
                },
                Decision::Remote,
                "gate: clean tree, remote present".to_string(),
                "command".to_string(),
            );
            let record = VerdictRecord::new(
                intent.run_id.clone(),
                verdict,
                ran,
                exit_code,
                "local".to_string(),
                None,
            );
            RunEntry {
                intent,
                verdict: Some(record),
            }
        }

        fn fired_map(
            stderr: &str,
            child_exit: i32,
            entries: &[RunEntry],
        ) -> HashMap<&'static str, bool> {
            drill_links(stderr, child_exit, entries)
                .into_iter()
                .collect()
        }

        #[test]
        fn every_link_of_the_real_chain_fires() {
            let links = drill_links(
                &chain_stderr(),
                0,
                &[drill_entry(0, Verdict::Pass, RanLocation::LocalAfterInfra)],
            );
            assert_eq!(links.len(), 7, "the chain has seven asserted links");
            for (name, fired) in &links {
                assert!(fired, "link `{name}` must fire on the real chain");
            }
        }

        #[test]
        fn nothing_fires_without_the_chain() {
            // Empty stderr and an empty ledger delta: no link may claim
            // success — the drill must not pass vacuously.
            for (name, fired) in drill_links("", 0, &[]) {
                assert!(!fired, "link `{name}` fired on empty evidence");
            }
        }

        #[test]
        fn a_real_submission_does_not_satisfy_the_drill_links() {
            // A run that really submitted (no drill handle, no banner) and
            // degraded locally: the injection and banner links stay dark.
            let stderr = "[gantry] submitted: run-173-42\n\
                          [gantry] infra: wait failed: x\n\
                          [gantry] falling back to capped local run\n";
            let fired = fired_map(
                stderr,
                0,
                &[drill_entry(0, Verdict::Pass, RanLocation::LocalAfterInfra)],
            );
            assert!(!fired["the drill backend took the submission"]);
            assert!(!fired["the synthetic failure raised the timeout banner"]);
            assert!(!fired["the degrade named the drill as its infra reason"]);
            // …but the records and the capped run genuinely happened.
            assert!(fired["the semaphore-gated ladder fell back to a capped local run"]);
            assert!(fired["the write-ahead intent recorded decision: remote"]);
            assert!(fired["a terminal verdict recorded ran: local_after_infra"]);
        }

        #[test]
        fn a_lying_exit_code_fails_the_faithfulness_link() {
            // Child exited 7 while the capped run recorded 0: INV-3 broken,
            // and the drill must say so even though everything else fired.
            let fired = fired_map(
                &chain_stderr(),
                7,
                &[drill_entry(0, Verdict::Pass, RanLocation::LocalAfterInfra)],
            );
            assert!(!fired["the exit code is the capped local run's own (Pass)"]);
        }

        #[test]
        fn a_failing_capped_run_is_not_a_faithful_drill_exit() {
            // The drill's argv is `cargo --version`: a capped run that
            // itself failed means the chain degraded into a failure the
            // caller never asked for — the faithfulness link (record is
            // Pass AND codes match) stays dark either way.
            let fired = fired_map(
                &chain_stderr(),
                1,
                &[drill_entry(
                    1,
                    Verdict::TestFailure,
                    RanLocation::LocalAfterInfra,
                )],
            );
            assert!(!fired["the exit code is the capped local run's own (Pass)"]);
        }

        #[test]
        fn a_remote_ran_verdict_does_not_close_the_drill_run() {
            // A verdict claiming the work ran remotely would mean the
            // synthetic failure never degraded — the records link must
            // reject it rather than let the drill pass on a lie.
            let fired = fired_map(
                &chain_stderr(),
                0,
                &[drill_entry(0, Verdict::Pass, RanLocation::Remote)],
            );
            assert!(!fired["a terminal verdict recorded ran: local_after_infra"]);
        }

        #[test]
        fn the_link_set_is_exactly_the_reported_chain() {
            // Guard against a link being added or dropped without this
            // suite and the parent report disagreeing about the chain.
            let names: Vec<&'static str> = drill_links("", 0, &[])
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            assert_eq!(names.len(), 7);
            assert!(names.contains(&"the drill backend took the submission"));
            assert!(names.contains(&"the synthetic failure raised the timeout banner"));
            assert!(names.contains(&"the degrade named the drill as its infra reason"));
            assert!(names.contains(&"the semaphore-gated ladder fell back to a capped local run"));
            assert!(names.contains(&"the write-ahead intent recorded decision: remote"));
            assert!(names.contains(&"a terminal verdict recorded ran: local_after_infra"));
            assert!(names.contains(&"the exit code is the capped local run's own (Pass)"));
        }
    }
}
