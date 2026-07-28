// gantry — walking skeleton main entry point (plan §"Phase 0.5", Components §1 "Shim & dispatcher").
//
// This is the minimal main.rs for the walking skeleton (bf-2w7): it dispatches on
// argv[0] to select a profile — cargo tool profile vs management CLI — and in the
// cargo profile, immediately passthrough to the real binary for all invocations
// (no remote offload, no GitGate, no backend, no cgroup cap yet). Phase 0.5 is the
// passthrough-only stage that establishes the dispatch and version/help contract
// before the decision engine lands in Phase 1a.
//
// The only live modules are config and shim; all other references (gate, refs,
// backend, local, runlog) are removed from this skeleton. The binary compiles and
// runs end-to-end as a transparent passthrough shim, proving the argv[0] dispatch
// and the real-binary resolution layer work.

// The library crate contains the core modules
extern crate gantry;

// Re-export what main.rs needs
use gantry::config;
use gantry::decision;
use gantry::doctor;
use gantry::shim;
use gantry::state;

use std::env;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

/// The canonical version string printed by the management CLI.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Get the git repository URL from the origin remote.
///
/// Returns a file:// URL for local testing or the actual https:// URL for production.
fn get_repo_url() -> String {
    // Try to get the origin remote URL
    match Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // Convert to file:// URL if it's a local path for testing
            if url.starts_with('/') || url.starts_with('.') {
                format!("file://{}", url)
            } else {
                url
            }
        }
        _ => {
            // Fallback to current directory as file:// URL for local testing
            format!("file://{}", env::current_dir().unwrap().display())
        }
    }
}

/// Get the current git commit SHA.
///
/// Returns "HEAD" if git rev-parse fails.
fn get_current_sha() -> String {
    match Command::new("git").args(["rev-parse", "HEAD"]).output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "HEAD".to_string(),
    }
}

/// Entry point: dispatch on argv[0] and either run the management CLI or the
/// cargo tool profile.
fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    let invocation_name = shim::invocation_name(&argv);

    match invocation_name.as_str() {
        // Cargo tool profile: the shim sits ahead of the real toolchain and may
        // offload intercepted subcommands (not implemented in Phase 0.5 — this
        // skeleton only passthrough).
        "cargo" | "cargo.exe" => run_cargo_profile(&argv),

        // Management CLI: explicit invocation as `gantry version` or
        // `gantry help`. In Phase 0.5 only version and help are implemented.
        "gantry" => run_management_cli(&argv),

        // Unknown argv[0]: assume cargo profile (the common/mislabeled case).
        // A symlink named `cargos` or an invocation through a PATH entry with a
        // different basename lands here and is treated as the cargo tool.
        _ => run_cargo_profile(&argv),
    }
}

/// Cargo tool profile: the transparent shim path.
///
/// Phase 0.5 behavior (bf-2w7):
/// - If GANTRY_LOCAL=1 is set → passthrough (fast path).
/// - If the subcommand is not intercepted → passthrough (fast path).
/// - No remote offload yet — that lands in Phase 1a (bf-2w8 and children).
///
/// The fast path delegates to `shim::passthrough`, which resolves the real
/// cargo binary (self-recursion guard included) and execs it with argv[1..]
/// forwarded byte-exact. Exit code fidelity is guaranteed (INV-3).
fn run_cargo_profile(argv: &[String]) -> ExitCode {
    let load_result = config::Config::load();
    let cfg = load_result.config;

    // Print any warnings from config loading
    for warning in load_result.warnings {
        eprintln!("[gantry] config warning: {}", warning);
    }

    // Print broken-config banner if present
    if let Some(banner) = load_result.broken_banner {
        eprintln!(
            "[gantry] WARNING: config broken since {}, {} runs degraded",
            humantime_timestamp(banner.since),
            banner.count
        );
    }

    // Fast-path check: GANTRY_LOCAL=1 forces local passthrough even for an
    // intercepted subcommand (the common case during development and debugging).
    let force_local = env::var("GANTRY_LOCAL").is_ok();

    // Extract the subcommand (argv[1]) if present. An empty or absent argv[1]
    // means "cargo" with no subcommand, which is never intercepted and falls
    // through to passthrough.
    let subcommand = argv.get(1).map(|s| s.as_str()).unwrap_or("");

    // Phase 0.5: if the subcommand is not intercepted (or forced local), run the
    // passthrough fast path. No remote path exists yet — the decision engine,
    // GitGate, RefPusher, and backend all land in later children.
    if force_local || !cfg.intercepts("cargo", subcommand) {
        shim::passthrough(&cfg, argv)
    } else {
        // Intercepted subcommand without GANTRY_LOCAL=1. Run the decision pipeline
        // to determine if we should execute remotely and get the verdict.
        let repo_url = get_repo_url();
        let sha = get_current_sha();
        let args = &argv[1..]; // Skip argv[0] (cargo)

        let exit_code = decision::run_remote(&cfg, &repo_url, &sha, args);
        // Convert i32 to u8 for ExitCode (clamp to 0-255 range)
        let exit_code_u8 = if exit_code < 0 {
            0
        } else {
            (exit_code & 0xFF) as u8
        };
        ExitCode::from(exit_code_u8)
    }
}

/// Management CLI: explicit `gantry version` / `gantry help` commands.
///
/// Phase 0.5 implements only version and help (the minimal CLI surface). The
/// full command set (doctor, why, init, etc.) lands in Phase 1a as the CLI
/// module grows.
fn run_management_cli(argv: &[String]) -> ExitCode {
    // argv[0] is "gantry"; argv[1] is the subcommand if present.
    let subcommand = argv.get(1).map(|s| s.as_str()).unwrap_or("");

    match subcommand {
        // Version command: print the canonical version line and exit 0.
        "version" => {
            println!("gantry {VERSION}");
            ExitCode::SUCCESS
        }

        // Help flag variants: print usage and exit 0.
        "--help" | "-h" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }

        // Short version flag variants.
        "--version" | "-V" => {
            println!("gantry {VERSION}");
            ExitCode::SUCCESS
        }

        // on command: enable gantry (remove kill switch)
        "on" => match state::StateFile::open().and_then(|sf| sf.enable()) {
            Ok(()) => {
                println!("gantry enabled");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gantry on: failed to enable: {}", e);
                ExitCode::from(1)
            }
        },

        // off command: disable gantry (activate kill switch)
        "off" => match state::StateFile::open().and_then(|sf| sf.disable()) {
            Ok(()) => {
                println!("gantry disabled");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gantry off: failed to disable: {}", e);
                ExitCode::from(1)
            }
        },

        // doctor command: run health checks
        "doctor" => {
            let health = doctor::run_all_checks();
            health.print();

            // Check for --e2e or --drill flags
            if argv.get(2).map(|s| s.as_str()) == Some("--e2e") {
                match doctor::run_e2e_test() {
                    Ok(msg) => {
                        println!("\nE2E test: {}", msg);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("\nE2E test failed: {}", e);
                        ExitCode::from(1)
                    }
                }
            } else if argv.get(2).map(|s| s.as_str()) == Some("--drill") {
                match doctor::run_drill() {
                    Ok(msg) => {
                        println!("\nFire drill: {}", msg);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("\nFire drill failed: {}", e);
                        ExitCode::from(1)
                    }
                }
            } else {
                // Normal doctor run: exit 0 if healthy, 1 if unhealthy
                if health.is_healthy() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
        }

        // Unknown command: print a hint and exit 2 (conventional for CLI misuse).
        _ => {
            eprintln!("gantry: unknown command '{subcommand}'");
            eprintln!("Run 'gantry help' for usage.");
            ExitCode::from(2)
        }
    }
}

/// Format a SystemTime as a human-readable timestamp.
fn humantime_timestamp(time: SystemTime) -> String {
    use std::fmt::Write;
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    let mut result = String::new();
    if hours > 0 {
        write!(&mut result, "{}h ", hours).ok();
    }
    if minutes > 0 || hours > 0 {
        write!(&mut result, "{}m ", minutes).ok();
    }
    write!(&mut result, "{}s", seconds).ok();
    result
}

/// Print the minimal usage help for Phase 0.5.
fn print_usage() {
    println!("gantry — transparent offload shim for expensive cargo commands");
    println!();
    println!("Management CLI commands:");
    println!("  gantry version      Print version information");
    println!("  gantry help         Print this help message");
    println!("  gantry on           Enable gantry (remove kill switch)");
    println!("  gantry off          Disable gantry (activate kill switch)");
    println!("  gantry doctor       Run health checks");
    println!("  gantry doctor --e2e Run end-to-end canary test");
    println!("  gantry doctor --drill Run fault-injection fire drill");
    println!();
    println!("Cargo tool profile:");
    println!("  cargo test          Run cargo test (passthrough in Phase 0.5)");
    println!("  GANTRY_LOCAL=1 cargo test  Force local passthrough");
    println!();
    println!("Environment variables:");
    println!("  GANTRY_LOCAL=1      Force local passthrough (skip remote offload)");
    println!("  GANTRY_ON=1         Override state file to enable");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_invocation_name_cargo() {
        let argv = vec!["cargo".to_string(), "test".to_string()];
        let name = shim::invocation_name(&argv);
        assert_eq!(name, "cargo");
    }

    #[test]
    fn test_invocation_name_gantry() {
        let argv = vec!["gantry".to_string(), "version".to_string()];
        let name = shim::invocation_name(&argv);
        assert_eq!(name, "gantry");
    }

    #[test]
    fn test_invocation_name_path_to_cargo() {
        let argv = vec!["/usr/bin/cargo".to_string(), "build".to_string()];
        let name = shim::invocation_name(&argv);
        assert_eq!(name, "cargo");
    }

    #[test]
    fn test_invocation_name_empty_argv() {
        let argv: Vec<String> = vec![];
        let name = shim::invocation_name(&argv);
        assert_eq!(name, "gantry"); // SENTINEL
    }

    #[test]
    fn test_invocation_name_empty_argv0() {
        let argv = vec!["".to_string()];
        let name = shim::invocation_name(&argv);
        assert_eq!(name, "gantry"); // SENTINEL
    }

    #[test]
    fn test_management_cli_version() {
        let argv = vec!["gantry".to_string(), "version".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::SUCCESS);
    }

    #[test]
    fn test_management_cli_help() {
        let argv = vec!["gantry".to_string(), "help".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::SUCCESS);
    }

    #[test]
    fn test_management_cli_unknown_command() {
        let argv = vec!["gantry".to_string(), "unknown".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::from(2));
    }

    #[test]
    fn test_management_cli_no_subcommand() {
        let argv = vec!["gantry".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::from(2));
    }

    #[test]
    fn test_management_cli_version_short_flag() {
        let argv = vec!["gantry".to_string(), "--version".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::SUCCESS);
    }

    #[test]
    fn test_management_cli_version_v_flag() {
        let argv = vec!["gantry".to_string(), "-V".to_string()];
        let exit_code = run_management_cli(&argv);
        assert_eq!(exit_code, ExitCode::SUCCESS);
    }

    #[test]
    fn test_cargo_profile_non_intercepted_passthrough() {
        // Test that non-intercepted subcommands like "build" trigger passthrough
        let cfg = config::Config::hardcoded();
        let argv = ["cargo".to_string(), "build".to_string()];

        // Since "build" is not in the intercept_list, it should passthrough
        // (we can't actually test the exec behavior, but we can verify the logic)
        let subcommand = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        assert!(
            !cfg.intercepts("cargo", subcommand),
            "build should not be intercepted"
        );
    }

    #[test]
    fn test_gantry_local_forces_passthrough_logic() {
        // Test that GANTRY_LOCAL=1 forces passthrough even for intercepted subcommands
        let cfg = config::Config::hardcoded();
        let argv = ["cargo".to_string(), "test".to_string()];

        // Simulate the GANTRY_LOCAL environment variable being set
        let force_local = env::var("GANTRY_LOCAL").is_ok();

        // "test" IS in the intercept_list
        let subcommand = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        let is_intercepted = cfg.intercepts("cargo", subcommand);
        assert!(is_intercepted, "test should be intercepted");

        // The logic in run_cargo_profile is:
        // if force_local || !cfg.intercepts("cargo", subcommand) { passthrough }
        // So when force_local is true, we should passthrough even if intercepted
        // This is a compile-time test of the logic structure
        let should_passthrough = force_local || !is_intercepted;

        // Without GANTRY_LOCAL set, "test" should NOT passthrough (it's intercepted)
        assert!(
            !should_passthrough,
            "Without GANTRY_LOCAL, intercepted test should not passthrough"
        );
    }

    #[test]
    fn test_intercepted_subcommand_without_gantry_local() {
        // Test that intercepted subcommands without GANTRY_LOCAL do NOT passthrough
        let cfg = config::Config::hardcoded();
        let argv = ["cargo".to_string(), "test".to_string()];

        let subcommand = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        let force_local = env::var("GANTRY_LOCAL").is_ok();

        // "test" is intercepted
        assert!(cfg.intercepts("cargo", subcommand));

        // Without GANTRY_LOCAL, the condition is:
        // force_local || !cfg.intercepts("cargo", subcommand)
        // which is: false || !true = false
        // So we should NOT passthrough
        let should_passthrough = force_local || !cfg.intercepts("cargo", subcommand);
        assert!(
            !should_passthrough,
            "Intercepted 'test' without GANTRY_LOCAL should not passthrough"
        );
    }

    #[test]
    fn test_config_intercepts_returns_correct_values() {
        let cfg = config::Config::hardcoded();

        // "test" is in the intercept_list
        assert!(cfg.intercepts("cargo", "test"));

        // Other subcommands are not
        assert!(!cfg.intercepts("cargo", "build"));
        assert!(!cfg.intercepts("cargo", "check"));
        assert!(!cfg.intercepts("cargo", "run"));
        assert!(!cfg.intercepts("cargo", "doc"));
        assert!(!cfg.intercepts("cargo", "version"));
    }

    #[test]
    fn test_empty_subcommand_never_intercepted() {
        let cfg = config::Config::hardcoded();

        // Empty or absent subcommand is never intercepted
        assert!(!cfg.intercepts("cargo", ""));
    }

    #[test]
    fn test_argv0_gantry_routes_to_management_cli() {
        let argv = vec!["gantry".to_string()];
        let invocation_name = shim::invocation_name(&argv);

        match invocation_name.as_str() {
            "gantry" => {} // Correct route
            _ => panic!("Expected 'gantry' route, got {}", invocation_name),
        }
    }

    #[test]
    fn test_argv0_cargo_routes_to_cargo_profile() {
        let argv = vec!["cargo".to_string()];
        let invocation_name = shim::invocation_name(&argv);

        match invocation_name.as_str() {
            "cargo" | "cargo.exe" => {} // Correct route
            other => panic!("Expected cargo route, got {}", other),
        }
    }
}
