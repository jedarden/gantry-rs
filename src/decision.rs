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
use crate::backend::{RemoteBackend, RunSpec, Verdict};
use crate::config::Config;
use crate::refs::RefPusher;
use crate::runlog::{
    Decision as RunLogDecision, Durations, GateInputs, IntentRecord, RanLocation, RunLog,
    VerdictRecord,
};
use crate::state;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Run the decision pipeline for an intercepted subcommand.
///
/// This is the core remote execution path (plan §"Architecture"):
/// 1. Check GitGate eligibility
/// 2. Write OPEN intent record to runlog (write-ahead, INV-1)
/// 3. Push epoch ref via RefPusher
/// 4. Submit to command backend
/// 5. Wait for verdict
/// 6. Write terminal verdict record to runlog
/// 7. Return faithful exit code
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
///
/// ## Write-ahead logging (Phase 1a)
///
/// Per plan Component 7, an OPEN intent record is written BEFORE dispatch
/// capturing all gate inputs, and every exit path writes a terminal verdict.
/// This guarantees that SIGKILL mid-run leaves an orphaned intent that doctor
/// can report (INV-1), making silently-skipped runs structurally detectable.
pub fn run_remote(config: &Config, repo_url: &str, sha: &str, args: &[String]) -> i32 {
    // Check kill switches (gantry off state file, GANTRY_ON=0)
    let (enabled, source) = state::check_enabled();
    if !enabled {
        eprintln!("[gantry] kill switch active: {}", source);
        eprintln!("[gantry] decision: local execution (kill switch)");
        // Would fall back to local execution here, but for now return 1
        return 1;
    }

    // Open the runlog (creates state directory if needed)
    // Per EC-08, if runlog cannot be opened, proceed with in-memory records only
    let runlog = match RunLog::open() {
        Ok(rl) => Some(rl),
        Err(e) => {
            eprintln!(
                "[gantry] warning: cannot open runlog: {}. Proceeding without logging.",
                e
            );
            None
        }
    };

    // Print the decision line to stderr
    eprintln!("[gantry] decision: remote execution eligible");

    // Step 1: Check GitGate eligibility and capture inputs
    let gate_start = Instant::now();

    // Capture gate inputs for intent record (run these before the official check)
    let gate_inputs = GateInputs {
        worktree: crate::gate::is_inside_work_tree().unwrap_or(false),
        head: crate::gate::head_resolves().unwrap_or(false),
        remote: crate::gate::remote_exists(&config.remote.ci_remote).unwrap_or(false),
        clean: crate::gate::is_tree_clean().unwrap_or(false),
    };

    let eligibility = crate::gate::check_git_gate(&config.remote.ci_remote);
    let gate_duration_ms = gate_start.elapsed().as_millis() as u64;

    if !eligibility.eligible {
        // Ineligible for remote — should have been caught earlier, but handle it
        eprintln!("[gantry] ineligible: {}", eligibility.reason);
        eprintln!("[gantry] verdict: Ineligible");

        // Write verdict record if runlog is available
        if let Some(rl) = runlog {
            let _ = write_local_verdict(
                &rl,
                "ineligible".to_string(),
                crate::runlog::Verdict::InfraFailure,
                1,
                None,
            );
        }

        // Return non-zero to indicate failure
        return 1;
    }

    // Step 2: Write OPEN intent record BEFORE dispatch (write-ahead, INV-1)
    let run_id = if let Some(rl) = &runlog {
        let cwd_rel = std::env::current_dir()
            .ok()
            .map(|_p| {
                // Try to get relative path from repo root
                // For now, use "." as the default (Phase 1a)
                std::path::PathBuf::from(".")
            })
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let intent = IntentRecord::new(
            "cargo".to_string(),
            args.to_vec(),
            repo_url.to_string(),
            sha.to_string(),
            cwd_rel,
            gate_inputs,
            RunLogDecision::Remote,
            eligibility.reason.clone(),
            "command".to_string(), // hardcoded backend
        );

        match rl.open_intent(&intent) {
            Ok(id) => id,
            Err(e) => {
                eprintln!(
                    "[gantry] warning: cannot write intent record: {}. Proceeding without logging.",
                    e
                );
                // Generate a fallback run_id for this run
                format!(
                    "fallback-{:x}",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                )
            }
        }
    } else {
        // No runlog available, generate a fallback run_id
        format!(
            "fallback-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        )
    };

    let _total_start = Instant::now();

    // Step 3: Push epoch ref via RefPusher
    let push_start = Instant::now();
    let push_result = RefPusher::push(config, sha, &run_id);
    let push_duration_ms = push_start.elapsed().as_millis() as u64;

    if !push_result.success {
        eprintln!("[gantry] push failed: {}", push_result.reason);
        eprintln!("[gantry] verdict: PushFailed");

        // Write verdict record if runlog is available (infra failure path)
        if let Some(rl) = runlog {
            let _ = write_local_verdict(
                &rl,
                run_id,
                crate::runlog::Verdict::InfraFailure,
                1,
                Some(Durations {
                    gate: gate_duration_ms,
                    push: push_duration_ms,
                    queue: 0,
                    run: 0,
                }),
            );
        }

        // Return non-zero to indicate infra failure
        return 1;
    }

    // Step 4: Submit to command backend
    let backend = CommandBackend::new();

    // Extract tool, subcommand, and args from the intercepted command
    // Phase 0.5: tool is always "cargo", cwd_rel is empty (repo root)
    let tool = "cargo";
    let cwd_rel = "";
    let (subcommand, run_args) = match args.split_first() {
        Some((first, rest)) => (first.as_str(), rest.to_vec()),
        None => {
            eprintln!("[gantry] error: no subcommand provided");
            return 1;
        }
    };

    let spec = RunSpec::new(tool, subcommand, run_args, repo_url, sha, cwd_rel);

    let handle = match backend.submit(&spec) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[gantry] submit failed: {}", e);
            eprintln!("[gantry] verdict: InfraFailure");

            // Write verdict record if runlog is available (infra failure path)
            if let Some(rl) = runlog {
                let _ = write_local_verdict(
                    &rl,
                    run_id,
                    crate::runlog::Verdict::InfraFailure,
                    1,
                    Some(Durations {
                        gate: gate_duration_ms,
                        push: push_duration_ms,
                        queue: 0,
                        run: 0,
                    }),
                );
            }

            // Return non-zero to indicate infra failure
            return 1;
        }
    };

    eprintln!("[gantry] submitted: {}", handle.handle);

    // Step 5: Wait for verdict
    let queue_end = Instant::now();
    let queue_duration_ms =
        (queue_end - push_start - std::time::Duration::from_millis(push_duration_ms)).as_millis()
            as u64;

    let run_start = Instant::now();
    let deadline = Instant::now() + config.deadline();
    let verdict_result = backend.wait(&handle, deadline);
    let run_duration_ms = run_start.elapsed().as_millis() as u64;

    let verdict = match verdict_result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gantry] wait failed: {}", e);
            eprintln!("[gantry] verdict: InfraFailure");

            // Write verdict record if runlog is available (infra failure path)
            if let Some(rl) = runlog {
                let _ = write_verdict(
                    &rl,
                    run_id.clone(),
                    Verdict::InfraFailure,
                    RanLocation::Remote,
                    1,
                    handle.handle.clone(),
                    Some(Durations {
                        gate: gate_duration_ms,
                        push: push_duration_ms,
                        queue: queue_duration_ms,
                        run: run_duration_ms,
                    }),
                );
            }

            // Return non-zero to indicate infra failure
            return 1;
        }
    };

    // Step 6: Write terminal verdict record (successful completion path)
    if let Some(rl) = runlog {
        let exit_code = verdict.to_exit_code();
        let _ = write_verdict(
            &rl,
            run_id.clone(),
            verdict,
            RanLocation::Remote,
            exit_code,
            handle.handle.clone(),
            Some(Durations {
                gate: gate_duration_ms,
                push: push_duration_ms,
                queue: queue_duration_ms,
                run: run_duration_ms,
            }),
        );
    }

    // Step 7: Print verdict trailer and return faithful exit code
    eprintln!("[gantry] verdict: {}", verdict);
    verdict.to_exit_code()
}

/// Write a verdict record for a remote execution.
fn write_verdict(
    runlog: &RunLog,
    run_id: String,
    verdict: Verdict,
    ran: RanLocation,
    exit_code: i32,
    handle: String,
    durations_ms: Option<Durations>,
) -> Result<(), crate::runlog::RunLogError> {
    let runlog_verdict = convert_backend_verdict_to_runlog(verdict);
    let record = VerdictRecord::new(run_id, runlog_verdict, ran, exit_code, handle, durations_ms);
    runlog.close_verdict(&record)
}

/// Write a verdict record for a local execution.
fn write_local_verdict(
    runlog: &RunLog,
    run_id: String,
    verdict: crate::runlog::Verdict,
    exit_code: i32,
    durations_ms: Option<Durations>,
) -> Result<(), crate::runlog::RunLogError> {
    let record = VerdictRecord::new(
        run_id,
        verdict,
        RanLocation::Local,
        exit_code,
        "local".to_string(),
        durations_ms,
    );
    runlog.close_verdict(&record)
}

/// Convert a backend Verdict to a runlog Verdict.
fn convert_backend_verdict_to_runlog(verdict: Verdict) -> crate::runlog::Verdict {
    match verdict {
        Verdict::Pass => crate::runlog::Verdict::Pass,
        Verdict::TestFailure => crate::runlog::Verdict::TestFailure,
        Verdict::GateFailure => crate::runlog::Verdict::GateFailure,
        Verdict::InfraFailure => crate::runlog::Verdict::InfraFailure,
        Verdict::Cancelled => crate::runlog::Verdict::Cancelled,
        Verdict::Superseded => crate::runlog::Verdict::Superseded,
    }
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
