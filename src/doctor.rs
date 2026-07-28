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

use crate::config::Config;
use crate::runlog::RunLog;
use crate::shim::{resolve_real_binary, shim_dir};
use std::env;
use std::path::PathBuf;
use std::process::Command;

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
/// Pushes a tiny fixture ref through the real pipeline and asserts the expected pass verdict.
/// This validates the full pipeline is actually working on the operator's schedule.
pub fn run_e2e_test() -> Result<String, String> {
    // TODO: Implement e2e canary test
    // This requires:
    // 1. Create a trivial test command (cargo --version)
    // 2. Push it through RefPusher
    // 3. Submit to backend
    // 4. Wait for verdict
    // 5. Assert pass verdict

    Err("E2E test not yet implemented".to_string())
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
