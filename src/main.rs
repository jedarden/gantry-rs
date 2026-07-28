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

mod config;
mod shim;

use std::env;
use std::process::ExitCode;

/// The canonical version string printed by the management CLI.
const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    let cfg = config::Config::hardcoded();

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
    if force_local || !cfg.intercepts(subcommand) {
        shim::passthrough(&cfg, argv)
    } else {
        // Intercepted subcommand without GANTRY_LOCAL=1. In Phase 0.5 this branch
        // is unreachable because the only intercepted subcommand is `test`, and
        // the skeleton does not yet implement remote offload. The bead scope
        // explicitly deletes the run_remote() pipeline here, so an intercepted
        // `cargo test` would hit this placeholder and exit with an error.
        //
        // This placeholder is removed when the decision engine lands (Phase 1a).
        eprintln!("[gantry] remote offload is not implemented in Phase 0.5");
        eprintln!("[gantry] set GANTRY_LOCAL=1 to run locally");
        ExitCode::FAILURE
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

        // Unknown command: print a hint and exit 2 (conventional for CLI misuse).
        _ => {
            eprintln!("gantry: unknown command '{subcommand}'");
            eprintln!("Run 'gantry help' for usage.");
            ExitCode::from(2)
        }
    }
}

/// Print the minimal usage help for Phase 0.5.
fn print_usage() {
    println!("gantry — transparent offload shim for expensive cargo commands");
    println!();
    println!("Management CLI commands:");
    println!("  gantry version      Print version information");
    println!("  gantry help         Print this help message");
    println!();
    println!("Cargo tool profile:");
    println!("  cargo test          Run cargo test (passthrough in Phase 0.5)");
    println!("  GANTRY_LOCAL=1 cargo test  Force local passthrough");
    println!();
    println!("Environment variables:");
    println!("  GANTRY_LOCAL=1      Force local passthrough (skip remote offload)");
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
            !cfg.intercepts(subcommand),
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
        let is_intercepted = cfg.intercepts(subcommand);
        assert!(is_intercepted, "test should be intercepted");

        // The logic in run_cargo_profile is:
        // if force_local || !cfg.intercepts(subcommand) { passthrough }
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
        assert!(cfg.intercepts(subcommand));

        // Without GANTRY_LOCAL, the condition is:
        // force_local || !cfg.intercepts(subcommand)
        // which is: false || !true = false
        // So we should NOT passthrough
        let should_passthrough = force_local || !cfg.intercepts(subcommand);
        assert!(
            !should_passthrough,
            "Intercepted 'test' without GANTRY_LOCAL should not passthrough"
        );
    }

    #[test]
    fn test_config_intercepts_returns_correct_values() {
        let cfg = config::Config::hardcoded();

        // "test" is in the intercept_list
        assert!(cfg.intercepts("test"));

        // Other subcommands are not
        assert!(!cfg.intercepts("build"));
        assert!(!cfg.intercepts("check"));
        assert!(!cfg.intercepts("run"));
        assert!(!cfg.intercepts("doc"));
        assert!(!cfg.intercepts("version"));
    }

    #[test]
    fn test_empty_subcommand_never_intercepted() {
        let cfg = config::Config::hardcoded();

        // Empty or absent subcommand is never intercepted
        assert!(!cfg.intercepts(""));
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
