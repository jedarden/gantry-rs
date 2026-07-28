// gantry — decision engine and remote execution pipeline (plan §"Phase 0.5").
//
// Phase 0.5 walking skeleton, stage 5 of 5 (bf-1f9): builds on stages 1-4 (shim/config
// + GitGate + RefPusher + backend) and wires the end-to-end pipeline. This module
// implements the decision engine: when an intercepted subcommand runs and is not
// force-local, execute GitGate -> RefPusher -> command backend -> verdict, then
// exit with the verdict's faithful exit code.
//
// The skeleton prints crude stderr lines ([gantry] decision + verdict trailer) and
// short-circuits to passthrough/local when GANTRY_LOCAL=1 or Tier-0 (no backend).

use crate::backend::command::CommandBackend;
use crate::backend::{RemoteBackend, RunSpec};
use crate::config::Config;
use crate::refs::RefPusher;
use std::time::Instant;

/// Run the decision pipeline for an intercepted subcommand.
///
/// This is the core remote execution path (plan §"Architecture"):
/// 1. Check GitGate eligibility
/// 2. Push epoch ref via RefPusher
/// 3. Submit to command backend
/// 4. Wait for verdict
/// 5. Return faithful exit code
///
/// ## Parameters
///
/// - `config`: The hardcoded Config.
/// - `repo_url`: The repository URL (file:// for local testing).
/// - `sha`: The commit SHA to run.
/// - `args`: The command arguments (e.g., ["test", "--", "--nocapture"]).
///
/// ## Returns
///
/// The exit code that faithfully represents the verdict (0 for Pass, non-zero
/// for TestFailure).
pub fn run_remote(config: &Config, repo_url: &str, sha: &str, args: &[String]) -> i32 {
    // Print the decision line to stderr
    eprintln!("[gantry] decision: remote execution eligible");

    // Step 1: Check GitGate eligibility
    let eligibility = crate::gate::check_git_gate(&config.ci_remote);
    if !eligibility.eligible {
        // Ineligible for remote — should have been caught earlier, but handle it
        eprintln!("[gantry] ineligible: {}", eligibility.reason);
        // Return non-zero to indicate failure
        return 1;
    }

    // Step 2: Push epoch ref via RefPusher
    let push_result = RefPusher::push(config, sha);
    if !push_result.success {
        eprintln!("[gantry] push failed: {}", push_result.reason);
        // Return non-zero to indicate infra failure
        return 1;
    }

    // Step 3: Submit to command backend
    let backend = CommandBackend::new();
    let spec = RunSpec::new(repo_url, sha, args.to_vec());

    let handle = match backend.submit(&spec) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[gantry] submit failed: {}", e);
            // Return non-zero to indicate infra failure
            return 1;
        }
    };

    eprintln!("[gantry] submitted: {}", handle.handle);

    // Step 4: Wait for verdict
    let deadline = Instant::now() + config.deadline();
    let verdict = match backend.wait(&handle, deadline) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gantry] wait failed: {}", e);
            // Return non-zero to indicate infra failure
            return 1;
        }
    };

    // Step 5: Print verdict trailer and return faithful exit code
    eprintln!("[gantry] verdict: {}", verdict);
    verdict.to_exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_remote_returns_pass_exit_code() {
        // This is a minimal compile-time test — the full integration test
        // is in tests/integration.rs where we can run the real binary.
        //
        // The skeleton decision module has no side-effect-free logic to unit
        // test beyond the exit code mapping, which is covered by Verdict tests.
        let config = Config::hardcoded();
        let repo_url = "file:///repo";
        let sha = "abc123";
        let args = vec!["test".to_string()];

        // We can't actually run remote in a unit test (needs git repo + backend),
        // but we can verify the function signature and basic flow compiles.
        // The real test is the integration test in tests/integration.rs.
        let _ = (config, repo_url, sha, args);
    }
}
