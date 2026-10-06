//! Integration coverage for `gantry doctor --e2e` (plan §8, Component 8):
//! the canary's full loopback round trip, and its failing-leg contract.
//!
//! The canary rides the loopback backend regardless of the configured
//! backend, so these tests need no external service — each one builds a
//! throwaway git remote and executor under the system temp dir and removes
//! them when it finishes. The green path compiles and runs the fixture's
//! real (dependency-free) suite through the embedded reference executor;
//! the failing path aims a known-bad cargo at the same pipeline and asserts
//! the error names the leg that broke.

use gantry::config::Config;
use gantry::doctor::{run_e2e_canary, E2eCanaryOptions};
use std::path::PathBuf;

/// Green path: one full round trip through the loopback pipeline — push the
/// fixture commit with the real RefPusher, clone the epoch ref with the
/// embedded reference executor, run the fixture's known-good `cargo test`,
/// map the exit through the verdict ladder — must come back Ok with the
/// pass wording, proving the pipeline answers "is it actually working"
/// without any external service (plan §8).
#[test]
fn e2e_canary_round_trips_the_loopback_pipeline() {
    let config = Config::hardcoded();
    let outcome = run_e2e_canary(&config, &E2eCanaryOptions::default())
        .expect("the known-good canary suite must round-trip to a pass");
    assert!(
        outcome.contains("round trip passed"),
        "the green path must report the pass, got: {outcome}"
    );
}

/// Failing path: a cargo that always exits 1 makes the executor's suite
/// "ran and failed" — wait maps exit 1 to `TestFailure`, and the canary must
/// exit the function with an error that names the verdict leg (the leg that
/// broke) rather than passing silently or blaming infrastructure.
#[test]
fn e2e_canary_failure_names_the_verdict_leg() {
    let config = Config::hardcoded();
    let opts = E2eCanaryOptions {
        // Resolved by sh from the executor's inherited PATH — coreutils'
        // `false` exits 1 for any argv, so the suite fails without any
        // toolchain or fixture sensitivity.
        exec_cargo: Some(PathBuf::from("false")),
    };
    let err = run_e2e_canary(&config, &opts)
        .expect_err("a failing canary suite must fail the check, not pass");
    assert!(
        err.contains("leg 'verdict'"),
        "the error must name the leg that failed, got: {err}"
    );
    assert!(
        err.contains("TestFailure"),
        "the error must carry the verdict class the suite earned, got: {err}"
    );
}
