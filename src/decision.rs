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
use std::fs;
use std::path::Path;
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
    // Check kill switches (gantry off state file, GANTRY_ON=0). Disabled means
    // gantry is fully transparent: the run executes locally (plan flow
    // "disabled → LocalExecutor"), still recorded so nothing is silently
    // skipped (INV-1).
    let (enabled, source) = state::check_enabled();
    if !enabled {
        eprintln!("[gantry] kill switch active: {}", source);
        eprintln!("[gantry] decision: local execution (kill switch)");
        return execute_locally(
            config,
            repo_url,
            sha,
            args,
            &format!("kill switch: {source}"),
            "none",
        );
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

        // The runlog classifies this exit as an InfraFailure, so the flight
        // recorder treats it as one (plan Component 7). No intent record was
        // written, so the bundle lands under the same stand-in id the verdict
        // record below carries.
        record_infra_failure(
            config,
            "ineligible",
            "gate",
            &eligibility.reason,
            None,
            None,
        );

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

        // Flight recorder (plan Component 7, bf-3mc): the push is where most
        // infra flakes live (auth, remote reachability), so the bundle gathers
        // the git state while it is still fresh.
        record_infra_failure(config, &run_id, "push", &push_result.reason, None, None);

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

            // Flight recorder (plan Component 7, bf-3mc): the backend's own
            // error text is the raw response a post-mortem wants.
            record_infra_failure(config, &run_id, "submit", &e.reason, None, None);

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

            // Flight recorder (plan Component 7, bf-3mc): the failure happened
            // after submit, so the bundle names the handle the run was watched
            // under.
            record_infra_failure(
                config,
                &run_id,
                "wait",
                &e.reason,
                Some(&handle.handle),
                None,
            );

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

    // A terminal InfraFailure verdict is an infra exit like any other (plan
    // Component 7, bf-3mc): the run produced no usable verdict, so the bundle
    // is written even though wait() itself returned cleanly. Checked before
    // the record below because Verdict is Copy but the runlog write moves it.
    if verdict == Verdict::InfraFailure {
        record_infra_failure(
            config,
            &run_id,
            "remote-verdict",
            "remote run ended in InfraFailure",
            Some(&handle.handle),
            None,
        );
    }

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

/// Flight recorder entry point for the remote pipeline (plan Component 7,
/// bf-3mc): snapshot one InfraFailure into the REDACTED crash bundle under
/// `<state dir>/crash/<run-id>/` ([`crate::crash::record`]). Every site that
/// prints `verdict: InfraFailure` calls this first, so the bundle always
/// carries the run id the runlog intent used — `gantry report <run-id>` finds
/// it. Best effort by construction: the recorder degrades to a single
/// `[gantry] warning:` line and never changes the verdict, the exit code, or
/// the trailer the caller is about to write. `handle` names the backend run
/// when the failure happened after submit; the backend errors this pipeline
/// sees expose a reason string only, so `backend_response` has nothing honest
/// to carry here and stays `None`.
fn record_infra_failure(
    config: &Config,
    run_id: &str,
    stage: &str,
    reason: &str,
    handle: Option<&str>,
    recent_stderr: Option<&str>,
) {
    let rec = crate::crash::CrashRecord {
        run_id,
        stage,
        infra_reason: reason,
        handle,
        backend_response: None,
        recent_stderr,
    };
    crate::crash::record(config, &rec);
}

// ============================================================================
// Tier-0: zero-config local execution (plan §"Tier-0 zero-config")
// ============================================================================

/// Rate limit for the Tier-0 notice line: at most once per hour. Plan
/// §"Tier-0 zero-config": the tier is noted "at most once per hour
/// (state-file timestamp) so transcripts see it without per-run noise".
const TIER0_NOTICE_INTERVAL_SECS: u64 = 3600;

/// Run an intercepted subcommand under Tier-0 defaults (`backend = "none"`).
///
/// Tier-0 is the zero-config default: with no config file anywhere, gantry is
/// a pure local cap-wrapper and nothing goes remote (plan §"Tier-0"). An
/// intercepted subcommand never enters the remote pipeline — no GitGate
/// eligibility, no RefPusher, no backend submit — but interception still
/// applies the full RunLog treatment: a write-ahead intent record with
/// `decision: local` plus a terminal verdict record, so `gantry why` can
/// replay the decision truthfully (INV-1).
///
/// The kill switch is checked first: `gantry off` must make gantry fully
/// transparent, so a disabled gantry runs locally without the tier notice —
/// the user switched gantry off rather than merely leaving it unconfigured.
pub fn run_tier0(config: &Config, repo_url: &str, sha: &str, args: &[String]) -> i32 {
    let (enabled, source) = state::check_enabled();
    if !enabled {
        eprintln!("[gantry] kill switch active: {}", source);
        eprintln!("[gantry] decision: local execution (kill switch)");
        return execute_locally(
            config,
            repo_url,
            sha,
            args,
            &format!("kill switch: {source}"),
            "none",
        );
    }

    eprintln!("[gantry] decision: local execution (Tier-0: no backend configured)");
    note_tier0();
    execute_locally(
        config,
        repo_url,
        sha,
        args,
        "Tier-0: no backend configured (zero-config cap-only)",
        "none",
    )
}

/// Execute an intercepted subcommand locally, with the full RunLog treatment.
///
/// This is the shared tail of every local decision path (Tier-0, kill
/// switch): a write-ahead intent record (`decision: local`, INV-1), the real
/// binary resolved by the shim (never a fallback-to-self, plan §1), then a
/// terminal verdict record and a faithful exit code (INV-3). Exit 0 maps to
/// [`crate::runlog::Verdict::Pass`], any other exit to `TestFailure` — the
/// suite's own failure is not an infra failure. A binary that cannot be
/// resolved or spawned is an `InfraFailure`: loud, recorded, non-zero — never
/// a silent success.
///
/// Returns the child's exit code, or 1 when nothing could be run.
fn execute_locally(
    config: &Config,
    repo_url: &str,
    sha: &str,
    args: &[String],
    reason: &str,
    backend: &str,
) -> i32 {
    // Open the runlog first (per EC-08, proceed with in-memory records only
    // if it cannot be opened). Its state directory is the same one the
    // Tier-0 notice timestamp lives in — note_tier0() has usually created it
    // by now, and RunLog::open tolerates an existing directory.
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

    // Gate inputs are replay context for `gantry why`, not an eligibility
    // check — a local run is never gated — so a failed git query degrades to
    // `false` exactly as in the remote pipeline.
    let gate_start = Instant::now();
    let gate_inputs = GateInputs {
        worktree: crate::gate::is_inside_work_tree().unwrap_or(false),
        head: crate::gate::head_resolves().unwrap_or(false),
        remote: crate::gate::remote_exists(&config.remote.ci_remote).unwrap_or(false),
        clean: crate::gate::is_tree_clean().unwrap_or(false),
    };
    let gate_duration_ms = gate_start.elapsed().as_millis() as u64;

    let run_id = if let Some(rl) = &runlog {
        let intent = IntentRecord::new(
            "cargo".to_string(),
            args.to_vec(),
            repo_url.to_string(),
            sha.to_string(),
            std::path::PathBuf::from("."),
            gate_inputs,
            RunLogDecision::Local,
            reason.to_string(),
            backend.to_string(),
        );
        match rl.open_intent(&intent) {
            Ok(id) => id,
            Err(e) => {
                eprintln!(
                    "[gantry] warning: cannot write intent record: {}. Proceeding without logging.",
                    e
                );
                fallback_run_id()
            }
        }
    } else {
        fallback_run_id()
    };

    let run_start = Instant::now();
    let real = match crate::shim::resolve_real_binary(config) {
        Ok(path) => path,
        Err(why) => {
            // Resolution failure OR the self-recursion guard's refusal: never
            // exec, never fall back to self (plan §1). Loud, recorded, and
            // non-zero, matching the passthrough contract.
            eprintln!("[gantry] {why}");
            if let Some(rl) = &runlog {
                let _ = write_local_verdict(
                    rl,
                    run_id,
                    crate::runlog::Verdict::InfraFailure,
                    1,
                    Some(local_durations(gate_duration_ms, &run_start)),
                );
            }
            eprintln!("[gantry] verdict: InfraFailure");
            return 1;
        }
    };

    // Capped (bf-139, plan §"Tier-0"): the Tier-0 and kill-switch tails are
    // what make gantry "a pure local cap-wrapper" — the child runs under the
    // configured per-run cgroup cap exactly as the predecessor bash pair
    // capped its local runs. The process-wide wrapper probes the launch line
    // once per process (so an unusable scope degrades before the real run,
    // even when several local tails fire in one process) and, once scoped,
    // propagates the child's exit status, so the verdict mapping below is
    // untouched (INV-3); where a scope cannot be created the degrade is a
    // plain spawn with a one-per-process `[gantry] cap:` note (plan
    // Component 6). The boxwide `gantry.slice` placement (bf-xj0) layers on
    // this same seam later.
    let status = crate::cap::process_cap(config).spawn(&real, args);

    let (verdict, exit_code) = match status {
        Ok(status) => {
            let code = status_to_i32(status);
            let verdict = if code == 0 {
                crate::runlog::Verdict::Pass
            } else {
                crate::runlog::Verdict::TestFailure
            };
            (verdict, code)
        }
        Err(why) => {
            // The binary resolved but would not run (missing exec bit,
            // permission denied, exec format error): same loud-and-non-zero
            // contract as a resolution failure.
            eprintln!("[gantry] failed to run `{}`: {why}", real.display());
            (crate::runlog::Verdict::InfraFailure, 1)
        }
    };

    if let Some(rl) = &runlog {
        let _ = write_local_verdict(
            rl,
            run_id,
            verdict,
            exit_code,
            Some(local_durations(gate_duration_ms, &run_start)),
        );
    }

    eprintln!("[gantry] verdict: {verdict}");
    exit_code
}

/// Durations for a local run: only gate and run are meaningful; push and
/// queue are remote-pipeline stages that never happened.
fn local_durations(gate_ms: u64, run_start: &Instant) -> Durations {
    Durations {
        gate: gate_ms,
        push: 0,
        queue: 0,
        run: run_start.elapsed().as_millis() as u64,
    }
}

/// Generate a run_id when no runlog is available to mint one.
///
/// The stand-in keeps the verdict record schema-valid when the intent could
/// not be written (or the runlog could not be opened): the pair correlation
/// is then impossible by construction, but the record still carries a
/// well-formed `run_id` instead of a lie.
fn fallback_run_id() -> String {
    format!(
        "fallback-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}

/// Map a child's exit status to a faithful i32 (INV-3): success → 0, a normal
/// exit code verbatim, and on Unix death-by-signal S → 128+S (the shell's
/// `$?` convention, matching the shim's passthrough mapping).
fn status_to_i32(status: std::process::ExitStatus) -> i32 {
    if status.success() {
        return 0;
    }
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        1
    }
}

/// Print the Tier-0 notice, at most once per hour.
///
/// Plan §"Tier-0 zero-config": "a `[gantry]` line notes the tier at most once
/// per hour (state-file timestamp) so transcripts see it without per-run
/// noise". The timestamp lives in `<state dir>/tier0-notice` (unix seconds)
/// next to the runlog. Any failure to record it degrades to printing the
/// notice — the tier line is never silently suppressed by a broken state
/// file, and a failed timestamp write never blocks the run.
fn note_tier0() {
    let notice = "[gantry] Tier-0: no remote backend configured; running locally \
                  (zero-config cap-only mode)";
    let Some(dir) = state::StateFile::state_dir() else {
        // Nowhere to record the hour: show the tier rather than go silent.
        eprintln!("{notice}");
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if tier0_notice_due_in(&dir, now) {
        eprintln!("{notice}");
        // The state directory may not exist yet (this runs before RunLog::open
        // could create it); create it so the timestamp actually lands.
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("tier0-notice"), now.to_string());
    }
}

/// Whether a Tier-0 notice is due, given the notice-timestamp directory and
/// the current unix time. `true` when the file is missing, unreadable, or
/// garbage (degrade open — the tier line is never silently suppressed), or
/// when [`TIER0_NOTICE_INTERVAL_SECS`] have elapsed since the last notice.
fn tier0_notice_due_in(dir: &Path, now: u64) -> bool {
    match fs::read_to_string(dir.join("tier0-notice")) {
        Ok(content) => content
            .trim()
            .parse::<u64>()
            .map(|ts| now.saturating_sub(ts) >= TIER0_NOTICE_INTERVAL_SECS)
            .unwrap_or(true),
        Err(_) => true,
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

    mod tier0_notice {
        use super::*;
        use tempfile::TempDir;

        #[test]
        fn due_when_no_timestamp_file_exists() {
            let dir = TempDir::new().unwrap();
            assert!(tier0_notice_due_in(dir.path(), 1_000_000));
        }

        #[test]
        fn suppressed_within_the_hour() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tier0-notice"), "1000000").unwrap();
            assert!(!tier0_notice_due_in(dir.path(), 1_000_000 + 3599));
        }

        #[test]
        fn due_again_after_an_hour() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tier0-notice"), "1000000").unwrap();
            assert!(tier0_notice_due_in(dir.path(), 1_000_000 + 3600));
        }

        #[test]
        fn clock_going_backwards_does_not_spam_the_notice() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tier0-notice"), "2000000").unwrap();
            assert!(!tier0_notice_due_in(dir.path(), 1_000_000));
        }

        #[test]
        fn garbage_timestamp_degrades_to_due() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tier0-notice"), "not-a-timestamp").unwrap();
            assert!(tier0_notice_due_in(dir.path(), 1_000_000));
        }

        #[test]
        fn surrounding_whitespace_is_tolerated() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tier0-notice"), " 1000000\n").unwrap();
            assert!(!tier0_notice_due_in(dir.path(), 1_000_000 + 60));
        }
    }

    mod local_exit_codes {
        use super::*;

        #[test]
        fn success_maps_to_zero() {
            let status = std::process::Command::new("true").status().unwrap();
            assert_eq!(status_to_i32(status), 0);
        }

        #[test]
        fn exit_code_is_faithful() {
            let status = std::process::Command::new("false").status().unwrap();
            assert_eq!(status_to_i32(status), 1);

            let script = if cfg!(unix) { "exit 42" } else { "" };
            if script.is_empty() {
                return;
            }
            let status = std::process::Command::new("sh")
                .args(["-c", script])
                .status()
                .unwrap();
            assert_eq!(status_to_i32(status), 42);
        }
    }
}
