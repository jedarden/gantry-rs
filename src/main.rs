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
