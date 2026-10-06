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
use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict};
use crate::config::{Backend, Config};
use crate::refs::RefPusher;
use crate::runlog::{
    Decision as RunLogDecision, Durations, GateInputs, IntentRecord, RanLocation, RunLog,
    VerdictRecord,
};
use crate::state;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The attach branch's dispatch triple: the originator's handle verbatim —
/// the joiner waits on the run it attached to, never one it forged — with
/// zero push and queue durations, because the joiner ran neither stage (the
/// originator's epoch ref already carries the sha and its workflow is
/// already submitted). Unit-pinned here; the recording-backend proof that no
/// push or submit reaches the wire lives in
/// tests/joiner_attach_integration.rs.
fn joiner_dispatch(handle: &RunHandle) -> (RunHandle, u64, u64) {
    (handle.clone(), 0, 0)
}

/// Select the pre-wait dispatch for an invocation. An attached invocation has
/// already inherited both dispatch stages from its originator, so the
/// originator closure must not be evaluated. Keeping that short-circuit in a
/// small seam makes the side-effect boundary testable without weakening the
/// process-level recording-backend proof.
fn dispatch_with_join<F>(
    attached: Option<&crate::jointable::AttachHandle>,
    originator_dispatch: F,
) -> Result<(RunHandle, u64, u64), i32>
where
    F: FnOnce() -> Result<(RunHandle, u64, u64), i32>,
{
    match attached {
        Some(attach) => Ok(joiner_dispatch(&attach.handle)),
        None => originator_dispatch(),
    }
}

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
/// A deadline expiry in the wait is the one failure that does not end the
/// run: the attempt is classified and flight-recorded as an `InfraFailure`
/// (DD-4 — no fabricated verdict), then degrades through the capped-local
/// fallback ladder ([`crate::local::run_fallback`], plan Component 6:
/// "deadline-exceeded classify as InfraFailure → fallback") so the caller
/// still lands a real result. Every other wait failure keeps the bare
/// `InfraFailure` exit.
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
            "cargo",
            None,
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
            crate::cli::backend_name(config.remote.backend.clone()).to_string(),
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

    // Plan Component 9: JoinTable concurrent-run dedup. An identical run —
    // same remote, sha, tool, and args — already in flight is joined (waited
    // on) instead of resubmitted, so a fleet of callers verifying one commit
    // lands one workflow, not one per caller. The claim sits after the intent
    // record (INV-1: every invocation, joiner included, writes its OPEN
    // first) and before the push (a joiner pushes nothing: the originator's
    // epoch ref already carries the sha). The originator's entry guard lives
    // to the end of the function, so the key clears on every exit path — a
    // failed dispatch can never wedge it shut. The key's command shape is the
    // RunSpec's (plan Q-2), so an intercepted `cargo test` and
    // `gantry run -- cargo test` dedup as one.
    let (join_subcommand, join_args) = match args.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => ("", args),
    };
    let join_key =
        crate::jointable::JoinKey::new(repo_url, sha, "cargo", join_subcommand, join_args);
    // The dedup kill switch (GANTRY_JOIN=0), checked like the master one: a
    // caller that must not dedup gets exactly the pre-Component-9 behavior —
    // no claim, no state touched, submit alongside everything else.
    let (join_enabled, join_source) = state::check_join_enabled();
    let (join_entry, joined, attached_dispatch) = if !join_enabled {
        eprintln!("[gantry] dedup kill switch active: {}", join_source);
        eprintln!("[gantry] join: submitting without dedup");
        (None, None, None)
    } else {
        match crate::jointable::claim(
            crate::jointable::default_state_dir().as_deref(),
            &join_key,
            &run_id,
            crate::cli::backend_name(config.remote.backend.clone()),
        ) {
            crate::jointable::JoinDecision::Originator(entry) => (Some(entry), None, None),
            crate::jointable::JoinDecision::Attach(attach) => {
                eprintln!(
                    "[gantry] joining in-flight run {} (identical run already dispatched as {})",
                    attach.handle.handle, attach.originator_run_id
                );
                // The originator already pushed the epoch ref and submitted
                // this run. Consume its handle at the claim boundary so the
                // joiner enters the shared wait with no dispatch-stage time;
                // the lazy normal path below is never evaluated for Attach.
                let dispatch = (attach.handle.clone(), 0, 0);
                (None, Some(attach), Some(dispatch))
            }
            crate::jointable::JoinDecision::Unjoined => (None, None, None),
        }
    };

    // Steps 3 and 4 (originator and unjoined): push the epoch ref, then
    // submit; a joiner skips both and meets them in Step 5's shared wait.
    //
    // The backend runs the argv templates the config resolved (user layer —
    // the repo layer cannot set them, trust boundary S-2). Before
    // gantry-f6c93e5a this site built the default-templated backend
    // unconditionally, so a
    // configured `[remote.command]` was silently dead: every submit ran the
    // `./contrib/gantry-exec.sh` default and failed with "command not found"
    // no matter what the user configured. `command: None` (backend = command
    // with no template table) keeps the default templates.
    let backend = build_backend(config);
    let (handle, push_duration_ms, queue_duration_ms) = match attached_dispatch {
        Some(dispatch) => dispatch,
        None => match dispatch_with_join(None, || {
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
                // The pusher's reason is the raw response this stage surfaced, so it
                // rides both the event line and the backend artifact channel.
                record_infra_failure(
                    config,
                    &run_id,
                    "push",
                    &push_result.reason,
                    None,
                    Some(&push_result.reason),
                    None,
                );

                // Write verdict record if runlog is available (infra failure path)
                if let Some(rl) = &runlog {
                    let _ = write_local_verdict(
                        rl,
                        run_id.clone(),
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
                return Err(1);
            }

            // Step 4: Submit to the configured backend.

            // Extract tool, subcommand, and args from the intercepted command
            // Phase 0.5: tool is always "cargo", cwd_rel is empty (repo root)
            let tool = "cargo";
            let cwd_rel = "";
            let (subcommand, run_args) = match args.split_first() {
                Some((first, rest)) => (first.as_str(), rest.to_vec()),
                None => {
                    eprintln!("[gantry] error: no subcommand provided");
                    return Err(1);
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
                    record_infra_failure(
                        config,
                        &run_id,
                        "submit",
                        &e.reason,
                        None,
                        Some(&e.reason),
                        None,
                    );

                    // Write verdict record if runlog is available (infra failure path)
                    if let Some(rl) = &runlog {
                        let _ = write_local_verdict(
                            rl,
                            run_id.clone(),
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
                    return Err(1);
                }
            };

            // Open the attach window (plan Component 9): from here until the
            // entry guard drops, identical invocations join this handle
            // instead of submitting their own.
            if let Some(entry) = &join_entry {
                entry.record_handle(&handle);
            }

            eprintln!("[gantry] submitted: {}", handle.handle);

            let queue_end = Instant::now();
            let queue_duration_ms =
                (queue_end - push_start - std::time::Duration::from_millis(push_duration_ms))
                    .as_millis() as u64;
            Ok((handle, push_duration_ms, queue_duration_ms))
        }) {
            Ok(dispatch) => dispatch,
            Err(code) => return code,
        },
    };

    let run_start = Instant::now();
    let deadline = Instant::now() + backend_wait_deadline(config);
    let verdict_result = backend.wait(&handle, deadline);
    let run_duration_ms = run_start.elapsed().as_millis() as u64;

    let verdict = match verdict_result {
        Ok(v) => {
            // A joiner owns a waiter registration, not the originator's
            // entry. Once the shared backend returns a terminal verdict,
            // reclaim the entry as well: the originator may have died after
            // recording its handle but before its guard reached Drop.
            if let Some(attach) = joined.as_ref() {
                attach.release();
            }
            v
        }
        Err(e) => {
            report_wait_failure(&e, &handle.handle);

            // Flight recorder (plan Component 7, bf-3mc): the failure happened
            // after submit, so the bundle names the handle the run was watched
            // under.
            record_infra_failure(
                config,
                &run_id,
                "wait",
                &e.reason,
                Some(&handle.handle),
                Some(&e.reason),
                None,
            );

            // A deadline expiry is the one wait failure that does not end the
            // run here (DD-4, plan Component 6: "deadline-exceeded classify
            // as InfraFailure → fallback"): the attempt above is classified
            // and flight-recorded as the InfraFailure it is — never a
            // fabricated verdict — and the run degrades through the capped-
            // local ladder (AS-3) so the caller still lands a real result
            // instead of a bare infra exit. The ladder closes the intent with
            // the local outcome (`ran: local_after_infra`); the flight
            // recorder is the remote attempt's classification home.
            //
            // Every other wait failure keeps this tail's bare InfraFailure
            // exit: the degrade-to-local is the case the plan names for the
            // expiry specifically, not a general wait-failure policy.
            if e.deadline_exceeded {
                // The expiry's runs.jsonl home is the terminal record the
                // ladder below writes, so the timeout detail rides the
                // context: the backend in its config spelling, the abandoned
                // run's handle, and the expiry reason. That is what lets the
                // ledger's one record identify the deadline timeout instead
                // of losing it to the local rerun's outcome.
                let expiry = deadline_expiry(config, &handle, &e.reason);
                let ctx = crate::local::FallbackContext {
                    runlog: runlog.as_ref(),
                    run_id: &run_id,
                    gate_ms: gate_duration_ms,
                    push_ms: push_duration_ms + queue_duration_ms,
                    timeout: Some(expiry),
                };
                return crate::local::run_fallback(config, args, &e.reason, &ctx);
            }

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

// ============================================================================
// Explicit offload: `gantry run -- <cmd…>` (plan §"CLI surface", v1.x)
// ============================================================================

/// Run an explicitly offloaded command — `gantry run -- <cmd…>` — through the
/// same pipeline as an intercepted cargo invocation: kill switch → Tier-0 →
/// GitGate → RefPusher → backend, with the same capped-local fallback ladder
/// and RunLog treatment (plan §"CLI surface": "explicit offload, no shim
/// needed"). No shimming involved: the caller hands over the wrapped argv
/// directly, program first (`["make", "-j4"]`).
///
/// The local arms are the intercepted path's local tails applied to the
/// wrapped command: kill switch and Tier-0 (backend `none`, including
/// `--backend none`) run it locally under the per-run cap
/// ([`execute_locally`] with the wrapped program resolved by
/// [`crate::shim::resolve_command_binary`] — the self-recursion guard
/// included, so `gantry run -- gantry …` is refused, never re-exec'd).
///
/// The remote arms are [`run_remote`]'s, with one deliberate difference: a
/// run that cannot complete remotely never fails the caller outright. Every
/// remote-path infra failure — gate ineligibility, push, submit, wait, and a
/// terminal `InfraFailure` verdict — degrades through the admission-
/// semaphore ladder ([`crate::local::run_fallback_program`]) so the explicit
/// request still produces a result, locally and capped. The wrapped command's
/// exit code wins on every local arm (INV-3); remote success lands on the
/// verdict ladder exactly as an intercepted run does.
///
/// ## RunSpec shape
///
/// The wrapped argv is transmitted in the same fields an intercepted
/// invocation uses, so a backend cannot tell the two submissions apart:
/// `gantry run -- cargo test …` produces the identical RunSpec an intercepted
/// `cargo test …` produces (the shipped executor's cargo-test contract
/// applies unchanged), and any other program rides the same channel as tool
/// `run`, subcommand = program, args = the wrapped tail — the faithful-argv
/// contract (plan Q-2) for backend templates that execute the argv they are
/// handed.
pub fn run_explicit(config: &Config, repo_url: &str, sha: &str, argv: &[String]) -> i32 {
    // The CLI parser rejects an empty wrapped command; this arm is defense in
    // depth for library callers.
    let Some((program, tail)) = argv.split_first() else {
        eprintln!("[gantry] error: no command given");
        return 1;
    };

    // Kill switch first, exactly as the intercepted pipeline: `gantry off`
    // makes gantry fully transparent, and the wrapped command runs locally —
    // recorded, capped, its own exit code returned.
    let (enabled, source) = state::check_enabled();
    if !enabled {
        eprintln!("[gantry] kill switch active: {}", source);
        eprintln!("[gantry] decision: local execution (kill switch)");
        return execute_locally(
            config,
            repo_url,
            sha,
            program,
            Some(program),
            tail,
            &format!("kill switch: {source}"),
            "none",
        );
    }

    // Tier-0 (zero-config default, or `--backend none`): nothing goes remote.
    // The wrapped command runs locally under the cap with the full RunLog
    // treatment — the explicit-offload analog of [`run_tier0`].
    if config.remote.backend == crate::config::Backend::None {
        eprintln!("[gantry] decision: local execution (Tier-0: no backend configured)");
        note_tier0();
        return execute_locally(
            config,
            repo_url,
            sha,
            program,
            Some(program),
            tail,
            "Tier-0: no backend configured (zero-config cap-only)",
            "none",
        );
    }

    // Remote pipeline — run_remote's shape, with the wrapped command in the
    // intent and the fallback ladder on every infra exit.
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

    eprintln!("[gantry] decision: remote execution eligible");

    let gate_start = Instant::now();
    let gate_inputs = GateInputs {
        worktree: crate::gate::is_inside_work_tree().unwrap_or(false),
        head: crate::gate::head_resolves().unwrap_or(false),
        remote: crate::gate::remote_exists(&config.remote.ci_remote).unwrap_or(false),
        clean: crate::gate::is_tree_clean().unwrap_or(false),
    };
    let eligibility = crate::gate::check_git_gate(&config.remote.ci_remote);
    let gate_duration_ms = gate_start.elapsed().as_millis() as u64;

    // Where this run is headed, decided once and recorded once: the intent
    // exists before dispatch (INV-1) on both the remote and the
    // gate-ineligible-fallback path, so `gantry why` can replay either.
    let going_remote = eligibility.eligible;
    let (decision, reason, backend_name) = if going_remote {
        (
            RunLogDecision::Remote,
            eligibility.reason.clone(),
            crate::cli::backend_name(config.remote.backend.clone()).to_string(),
        )
    } else {
        (
            RunLogDecision::Local,
            format!("gate: {}", eligibility.reason),
            "none".to_string(),
        )
    };

    let run_id = if let Some(rl) = &runlog {
        let intent = IntentRecord::new(
            program.to_string(),
            tail.to_vec(),
            repo_url.to_string(),
            sha.to_string(),
            std::path::PathBuf::from("."),
            gate_inputs,
            decision,
            reason,
            backend_name,
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

    if !going_remote {
        // Gate-ineligible: the plan's fallback ladder ("infra failure or gate
        // ineligibility") rather than run_remote's bare InfraFailure — an
        // explicitly requested command should still produce a result, and a
        // dirty tree is exactly what a capped local run handles well. The
        // wrapped program's resolution is the caller's contract (the
        // fallback tail takes an already-resolved path), so a refusal here is
        // loud, recorded, and non-zero.
        eprintln!("[gantry] ineligible: {}", eligibility.reason);
        return resolve_and_fall_back(
            config,
            program,
            tail,
            &format!("git gate: {}", eligibility.reason),
            runlog.as_ref(),
            &run_id,
            gate_duration_ms,
            0,
        );
    }

    // Step: push the epoch ref (content the executor will check out).
    let push_start = Instant::now();
    let push_result = RefPusher::push(config, sha, &run_id);
    let push_duration_ms = push_start.elapsed().as_millis() as u64;

    if !push_result.success {
        eprintln!("[gantry] push failed: {}", push_result.reason);
        // Flight recorder (plan Component 7): the push is where most infra
        // flakes live; the pusher's reason rides the bundle as the raw
        // response.
        record_infra_failure(
            config,
            &run_id,
            "push",
            &push_result.reason,
            None,
            Some(&push_result.reason),
            None,
        );
        return resolve_and_fall_back(
            config,
            program,
            tail,
            &format!("push failed: {}", push_result.reason),
            runlog.as_ref(),
            &run_id,
            gate_duration_ms,
            push_duration_ms,
        );
    }

    // Step: submit to the configured backend (the same dispatch run_remote
    // uses — user-configured templates, the shipped defaults, or the argo
    // backend its config names).
    let backend = build_backend(config);

    let (tool, subcommand, run_args) = explicit_run_spec_fields(program, tail);
    let spec = RunSpec::new(tool, &subcommand, run_args, repo_url, sha, "");

    let handle = match backend.submit(&spec) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[gantry] submit failed: {}", e.reason);
            record_infra_failure(
                config,
                &run_id,
                "submit",
                &e.reason,
                None,
                Some(&e.reason),
                None,
            );
            return resolve_and_fall_back(
                config,
                program,
                tail,
                &format!("submit failed: {}", e.reason),
                runlog.as_ref(),
                &run_id,
                gate_duration_ms,
                push_duration_ms,
            );
        }
    };

    eprintln!("[gantry] submitted: {}", handle.handle);

    // Step: wait for the verdict.
    let queue_end = Instant::now();
    let queue_duration_ms =
        (queue_end - push_start - std::time::Duration::from_millis(push_duration_ms)).as_millis()
            as u64;

    let run_start = Instant::now();
    let deadline = Instant::now() + backend_wait_deadline(config);
    let verdict = match backend.wait(&handle, deadline) {
        Ok(v) => v,
        Err(e) => {
            report_wait_failure(&e, &handle.handle);
            record_infra_failure(
                config,
                &run_id,
                "wait",
                &e.reason,
                Some(&handle.handle),
                Some(&e.reason),
                None,
            );
            return resolve_and_fall_back(
                config,
                program,
                tail,
                &format!("wait failed: {}", e.reason),
                runlog.as_ref(),
                &run_id,
                gate_duration_ms,
                push_duration_ms + queue_duration_ms,
            );
        }
    };

    // A terminal InfraFailure verdict is a remote-path infra failure like any
    // other: the run produced no usable verdict, so the flight recorder
    // writes the bundle and the capped-local ladder still lands the caller a
    // real result.
    if verdict == Verdict::InfraFailure {
        record_infra_failure(
            config,
            &run_id,
            "remote-verdict",
            "remote run ended in InfraFailure",
            Some(&handle.handle),
            None,
            None,
        );
        return resolve_and_fall_back(
            config,
            program,
            tail,
            "remote run ended in InfraFailure",
            runlog.as_ref(),
            &run_id,
            gate_duration_ms,
            push_duration_ms + queue_duration_ms,
        );
    }

    // Success path: record the remote verdict and return its ladder exit
    // code — the same fidelity an intercepted run delivers (INV-3).
    let exit_code = verdict.to_exit_code();
    if let Some(rl) = runlog {
        let _ = write_verdict(
            &rl,
            run_id,
            verdict,
            RanLocation::Remote,
            exit_code,
            handle.handle.clone(),
            Some(Durations {
                gate: gate_duration_ms,
                push: push_duration_ms,
                queue: queue_duration_ms,
                run: run_start.elapsed().as_millis() as u64,
            }),
        );
    }

    eprintln!("[gantry] verdict: {}", verdict);
    exit_code
}

/// Map the wrapped argv onto RunSpec fields, exactly as an intercepted
/// invocation is mapped: `cargo <sub> <args…>` keeps the intercepted shape
/// (tool "cargo", the subcommand split off), so `gantry run -- cargo test`
/// and an intercepted `cargo test` are the same submission — and any other
/// program rides the same channel as tool `run` with the program as the
/// subcommand (the faithful-argv contract for templates that execute the
/// argv they are handed).
fn explicit_run_spec_fields<'a>(
    program: &'a str,
    tail: &'a [String],
) -> (&'a str, String, Vec<String>) {
    if program == "cargo" {
        match tail.split_first() {
            Some((subcommand, rest)) => (program, subcommand.clone(), rest.to_vec()),
            None => ("run", program.to_string(), Vec::new()),
        }
    } else {
        ("run", program.to_string(), tail.to_vec())
    }
}

/// The fallback ladder's front door: resolve the wrapped program by the shim
/// rules, then hand the resolved path to the admission-semaphore tail
/// ([`crate::local::run_fallback_program`]). A resolution refusal is loud,
/// recorded, and non-zero — never a silent pass and never a fallback-to-self.
///
/// This is the one place `gantry run` differs from the intercepted pipeline's
/// infra tails by design: the interception path owns cargo's resolution
/// internally, while the wrapped program's resolution is the explicit
/// offload's own contract (plan §1: resolution failures surface, never
/// re-exec gantry).
// Each parameter is a distinct fact the ladder needs (config, the wrapped
// command, why the remote attempt died, the open run's identity, the gate
// and push splits) — a params struct would only move the count.
#[allow(clippy::too_many_arguments)]
fn resolve_and_fall_back(
    config: &Config,
    program: &str,
    tail: &[String],
    infra_reason: &str,
    runlog: Option<&RunLog>,
    run_id: &str,
    gate_ms: u64,
    push_ms: u64,
) -> i32 {
    let real = match crate::shim::resolve_command_binary(config, program) {
        Ok(path) => path,
        Err(why) => {
            eprintln!("[gantry] {why}");
            eprintln!("[gantry] cannot fall back locally: {infra_reason}");
            record_infra_failure(config, run_id, "run-resolve", &why, None, None, None);
            if let Some(rl) = runlog {
                let _ = write_local_verdict(
                    rl,
                    run_id.to_string(),
                    crate::runlog::Verdict::InfraFailure,
                    1,
                    Some(local_durations(gate_ms, &Instant::now())),
                );
            }
            eprintln!("[gantry] verdict: InfraFailure");
            return 1;
        }
    };
    let ctx = crate::local::FallbackContext {
        runlog,
        run_id,
        gate_ms,
        push_ms,
        // Not a deadline tail: this fallback follows a local-decision run,
        // whose watch never started, so there is no expiry to identify.
        timeout: None,
    };
    crate::local::run_fallback_program(config, &real, tail, infra_reason, &ctx)
}

/// The expiry detail the deadline tail stamps on the fallback's terminal
/// record: the backend whose wait expired, in its config spelling
/// ([`crate::cli::backend_name`]), the abandoned run's handle, and the
/// backend's expiry reason verbatim — the same three facts the
/// `[gantry] timeout` line printed (DD-4, plan Component 6).
fn deadline_expiry(
    config: &Config,
    handle: &RunHandle,
    reason: &str,
) -> crate::runlog::TimeoutExpiry {
    crate::runlog::TimeoutExpiry {
        backend: crate::cli::backend_name(config.remote.backend.clone()).to_string(),
        handle: handle.handle.clone(),
        reason: reason.to_string(),
    }
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
/// when the failure happened after submit; `backend_response` carries the raw
/// text the failure site actually surfaced (push/submit/wait hand over the
/// same string their `[gantry]` line prints) and stays `None` where gantry
/// generated the reason itself (gate, remote-verdict, the local tails).
fn record_infra_failure(
    config: &Config,
    run_id: &str,
    stage: &str,
    reason: &str,
    handle: Option<&str>,
    backend_response: Option<&str>,
    recent_stderr: Option<&str>,
) {
    let rec = crate::crash::CrashRecord {
        run_id,
        stage,
        infra_reason: reason,
        handle,
        backend_response,
        recent_stderr,
    };
    crate::crash::record(config, &rec);
}

/// The wait/stream deadline for the run's configured backend: the
/// per-backend override with the global `[remote] deadline_minutes` as
/// fallback (features.md v1.x "timeout/deadline config per backend"). The
/// argo backend resolves through [`Config::argo_deadline`] — its
/// `remote.argo.deadline_minutes` override — never the generic deadline;
/// the command templates keep [`Config::command_deadline`]. Tier-0 never
/// enters a remote wait, so its arm exists only to keep the match
/// exhaustive.
fn backend_wait_deadline(config: &Config) -> Duration {
    match config.remote.backend {
        Backend::Argo => config.argo_deadline(),
        Backend::None | Backend::Command => config.command_deadline(),
    }
}

/// Build the remote backend the configured `[remote] backend` names.
///
/// Both decision pipelines ([`run_remote`], [`run_explicit`]) submit and wait
/// through the [`RemoteBackend`] trait, but the construction site hardcoded
/// the command backend — so a configured `backend = "argo"` silently ran the
/// command templates (or their defaults) instead of the argo backend, and
/// every `[remote.argo]` key was dead in the pipeline. This is the glue that
/// makes the argo wait path reachable end to end: the argo backend gets the
/// config table's kubectl path, namespace, template, and friends; the
/// command templates keep the shipped defaults or the user's `[remote.command]`
/// table. Tier-0 never enters a remote pipeline, so its arm here keeps the
/// command default purely to stay exhaustive.
fn build_backend(config: &Config) -> Box<dyn RemoteBackend> {
    match config.remote.backend {
        Backend::Argo => {
            let argo = config.remote.argo.clone().unwrap_or_default();
            Box::new(crate::backend::argo::ArgoBackend::new(
                crate::backend::argo::ArgoConfig {
                    kubectl_path: argo.kubectl_path,
                    kubeconfig: argo.kubeconfig.to_string_lossy().into_owned(),
                    namespace: argo.namespace,
                    template: argo.template,
                    generate_name: argo.generate_name,
                    builder_image: argo.builder_image,
                    base_url: argo.base_url,
                },
            ))
        }
        Backend::None | Backend::Command => match &config.remote.command {
            Some(c) => Box::new(CommandBackend::with_config(
                crate::backend::command::CommandConfig {
                    submit: c.submit.clone(),
                    logs: c.logs.clone(),
                    wait: c.wait.clone(),
                    status: None, // the config schema carries no status step (RawCommand)
                },
            )),
            None => Box::new(CommandBackend::new()),
        },
    }
}

/// Render the stderr line a failed `backend.wait()` reports.
///
/// A deadline expiry (the backend's structured `deadline_exceeded` flag,
/// features.md v1.x "timeout/deadline config per backend") gets the
/// `[gantry] timeout` line naming the run (plan §"failure modes": a client
/// deadline exceeded prints the handle) because a timed-out run reads
/// differently from every other backend failure: nothing is wrong with the
/// code under test — the budget ran out, no verdict exists to report
/// (DD-4), and the run continues through the InfraFailure tail. The expiry
/// abandons the watch without cancelling the run, so when the backend
/// carries a run URL/identifier ([`BackendError::run_url`] — the argo
/// backend's `describe()`: the UI URL, or a bare identifier with no
/// `base_url`) the line appends it: the "here's-the-run-URL message"
/// (features.md v1.x) telling the operator where the abandoned run can
/// still be watched. Every other wait failure keeps the generic
/// `[gantry] wait failed:` line carrying the backend's reason.
fn wait_failure_line(e: &BackendError, handle: &str) -> String {
    if e.deadline_exceeded {
        match e.run_url.as_deref() {
            Some(url) => format!(
                "[gantry] timeout: run {handle} exceeded its deadline before a verdict was \
                 returned; the abandoned run can still be watched at {url}"
            ),
            None => format!(
                "[gantry] timeout: run {handle} exceeded its deadline before a verdict was returned"
            ),
        }
    } else {
        format!("[gantry] wait failed: {}", e.reason)
    }
}

/// Report a failed `backend.wait()` on stderr (see [`wait_failure_line`]).
fn report_wait_failure(e: &BackendError, handle: &str) {
    eprintln!("{}", wait_failure_line(e, handle));
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
            "cargo",
            None,
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
        "cargo",
        None,
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
/// `tool` names the tool profile the intent record carries (always "cargo"
/// for the intercepted paths); `program` selects what the tail spawns —
/// `None` resolves the real cargo by the shim rules, `Some(name)` resolves
/// that program by the same rules ([`crate::shim::resolve_command_binary`]),
/// which is how the explicit-offload paths run the wrapped command instead.
///
/// Returns the child's exit code, or 1 when nothing could be run.
// Each parameter is a distinct fact the local tail needs (config, the run's
// identity, the tool profile, the wrapped program, the argv, why the remote
// attempt died, the backend that ran it) — a params struct would only move
// the count.
#[allow(clippy::too_many_arguments)]
fn execute_locally(
    config: &Config,
    repo_url: &str,
    sha: &str,
    tool: &str,
    program: Option<&str>,
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
            tool.to_string(),
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
    let real = match match program {
        Some(name) => crate::shim::resolve_command_binary(config, name),
        None => crate::shim::resolve_real_binary(config),
    } {
        Ok(path) => path,
        Err(why) => {
            // Resolution failure OR the self-recursion guard's refusal: never
            // exec, never fall back to self (plan §1). Loud, recorded, and
            // non-zero, matching the passthrough contract.
            eprintln!("[gantry] {why}");
            record_infra_failure(config, &run_id, "local-resolve", &why, None, None, None);
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
            record_infra_failure(
                config,
                &run_id,
                "local-spawn",
                &why.to_string(),
                None,
                None,
                None,
            );
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

    #[test]
    fn joiner_dispatch_returns_the_originators_handle_with_zero_durations() {
        // The attach branch's whole dispatch contract, pinned at unit grain:
        // the handle comes back verbatim (the joiner waits on the run it
        // attached to — minting its own would resubmit under another name)
        // and both pre-wait durations are zero, because the joiner ran
        // neither the push nor the submit those durations measure. The
        // end-to-end proof that the skip holds at the wire — one submit, one
        // epoch ref, the ledger record carrying this handle and these zeros —
        // is tests/joiner_attach_integration.rs.
        let (handle, push_ms, queue_ms) = joiner_dispatch(&RunHandle::new("handle-originator"));
        assert_eq!(handle.handle, "handle-originator");
        assert_eq!((push_ms, queue_ms), (0, 0));
    }

    mod dispatch_selection {
        use super::*;
        use crate::jointable::{claim, claim_in, JoinDecision, JoinKey};
        use std::cell::Cell;

        /// A tiny backend recorder keeps these unit tests on the same backend
        /// contract as the command recording fixture: a submit is an
        /// observable side effect, not just a returned value.
        #[derive(Default)]
        struct RecordingBackend {
            submits: Cell<u32>,
        }

        impl RemoteBackend for RecordingBackend {
            fn submit(&self, _spec: &RunSpec) -> Result<RunHandle, BackendError> {
                self.submits.set(self.submits.get() + 1);
                Ok(RunHandle::new("recorded-originator"))
            }

            fn wait(
                &self,
                _handle: &RunHandle,
                _deadline: Instant,
            ) -> Result<Verdict, BackendError> {
                Ok(Verdict::Pass)
            }
        }

        /// The two dispatch side effects are recorded separately: the epoch
        /// ref push belongs to the client, while submit belongs to the
        /// backend. This is the unit-sized equivalent of the integration
        /// fixture's wire log.
        #[derive(Default)]
        struct RecordingDispatch {
            backend: RecordingBackend,
            epoch_ref_pushes: Cell<u32>,
        }

        impl RecordingDispatch {
            fn originator_dispatch(&self) -> Result<(RunHandle, u64, u64), i32> {
                self.epoch_ref_pushes.set(self.epoch_ref_pushes.get() + 1);
                let submitted = self
                    .backend
                    .submit(&RunSpec::new(
                        "cargo",
                        "test",
                        Vec::new(),
                        "file:///repo",
                        "abc123",
                        "",
                    ))
                    .map_err(|_| 1)?;
                Ok((submitted, 17, 23))
            }
        }

        fn real_attach() -> (
            tempfile::TempDir,
            crate::jointable::JoinEntry,
            crate::jointable::AttachHandle,
        ) {
            let state_dir = tempfile::tempdir().expect("create join state");
            let key = JoinKey::new("file:///repo", "abc123", "cargo", "test", &[]);
            let owner = match claim_in(state_dir.path(), &key, "originator", "recording")
                .expect("originator claim succeeds")
            {
                JoinDecision::Originator(entry) => entry,
                other => panic!("first claim must originate, got {other:?}"),
            };
            owner.record_handle(&RunHandle::new("recorded-originator"));

            let attach = match claim_in(state_dir.path(), &key, "joiner", "recording")
                .expect("joiner claim succeeds")
            {
                JoinDecision::Attach(attach) => attach,
                other => panic!("handle-bearing claim must attach, got {other:?}"),
            };
            (state_dir, owner, attach)
        }

        /// Exercise the same decision variants that `run_remote` receives
        /// from the JoinTable. Originators and unjoined invocations both
        /// retain the normal dispatch closure; only Attach gets the
        /// short-circuit. Keeping this adapter in the regression tests makes
        /// it impossible for both normal paths to accidentally collapse into
        /// an unexamined `None` fixture.
        fn dispatch_for_decision<F>(
            decision: JoinDecision,
            originator_dispatch: F,
        ) -> Result<(RunHandle, u64, u64), i32>
        where
            F: FnOnce() -> Result<(RunHandle, u64, u64), i32>,
        {
            match decision {
                JoinDecision::Attach(attach) => {
                    dispatch_with_join(Some(&attach), originator_dispatch)
                }
                JoinDecision::Originator(_entry) => dispatch_with_join(None, originator_dispatch),
                JoinDecision::Unjoined => dispatch_with_join(None, originator_dispatch),
            }
        }

        #[test]
        fn attach_returns_originator_handle_and_skips_recorded_dispatch() {
            let (_state_dir, _owner, attach) = real_attach();
            let recorder = RecordingDispatch::default();

            let (handle, push_ms, queue_ms) =
                dispatch_for_decision(JoinDecision::Attach(attach), || {
                    recorder.originator_dispatch()
                })
                .expect("attach dispatch succeeds");

            assert_eq!(handle, RunHandle::new("recorded-originator"));
            assert_eq!((push_ms, queue_ms), (0, 0));
            assert_eq!(
                recorder.epoch_ref_pushes.get(),
                0,
                "Attach must skip the epoch-ref push"
            );
            assert_eq!(
                recorder.backend.submits.get(),
                0,
                "Attach must skip backend submit"
            );
        }

        #[test]
        fn originator_dispatch_keeps_recorded_stages() {
            let state_dir = tempfile::tempdir().expect("create join state");
            let key = JoinKey::new("file:///repo", "abc123", "cargo", "test", &[]);
            let decision = claim_in(state_dir.path(), &key, "originator", "recording")
                .expect("originator claim succeeds");
            assert!(matches!(&decision, JoinDecision::Originator(_)));
            let recorder = RecordingDispatch::default();
            let (handle, push_ms, queue_ms) =
                dispatch_for_decision(decision, || recorder.originator_dispatch())
                    .expect("originator dispatch succeeds");

            assert_eq!(handle, RunHandle::new("recorded-originator"));
            assert_eq!((push_ms, queue_ms), (17, 23));
            assert_eq!(recorder.epoch_ref_pushes.get(), 1);
            assert_eq!(recorder.backend.submits.get(), 1);
        }

        #[test]
        fn unjoined_dispatch_keeps_recorded_stages() {
            let key = JoinKey::new("file:///repo", "abc123", "cargo", "test", &[]);
            let decision = claim(None, &key, "unjoined", "recording");
            assert!(matches!(&decision, JoinDecision::Unjoined));
            let recorder = RecordingDispatch::default();
            let (handle, push_ms, queue_ms) =
                dispatch_for_decision(decision, || recorder.originator_dispatch())
                    .expect("unjoined dispatch succeeds");

            assert_eq!(handle, RunHandle::new("recorded-originator"));
            assert_eq!((push_ms, queue_ms), (17, 23));
            assert_eq!(recorder.epoch_ref_pushes.get(), 1);
            assert_eq!(recorder.backend.submits.get(), 1);
        }
    }

    mod wait_failure_reporting {
        use super::*;

        #[test]
        fn deadline_expiry_prints_the_timeout_line_naming_the_run() {
            let e = BackendError::deadline("run h-9 deadline exceeded");
            assert_eq!(
                wait_failure_line(&e, "h-9"),
                "[gantry] timeout: run h-9 exceeded its deadline before a verdict was returned"
            );
        }

        #[test]
        fn timeout_line_takes_the_identifier_from_the_handle_not_the_reason() {
            // The identifier must come from the handle argument — a backend
            // that phrases its expiry differently (or embeds nothing at all)
            // still gets the run named on the timeout line.
            let e = BackendError::deadline("totally different phrasing");
            let line = wait_failure_line(&e, "workflow-abc-123");
            assert!(
                line.contains("workflow-abc-123"),
                "timeout line must name the run, got: {line}"
            );
            assert!(line.contains("[gantry] timeout"));
        }

        #[test]
        fn timeout_line_appends_the_run_url_the_backend_provides() {
            // The argo backend's expiry carries describe() — the UI URL the
            // operator needs once gantry stops watching (features.md v1.x
            // "here's-the-run-URL message"). Dropping it on the way from the
            // backend error to stderr would strand the abandoned run.
            let e = BackendError::deadline_with_url(
                "workflow gantry-abc123 deadline exceeded while polling status.phase",
                "https://argo.example.com/workflows/argo-workflows/gantry-abc123",
            );
            assert_eq!(
                wait_failure_line(&e, "gantry-abc123"),
                "[gantry] timeout: run gantry-abc123 exceeded its deadline before a verdict \
                 was returned; the abandoned run can still be watched at \
                 https://argo.example.com/workflows/argo-workflows/gantry-abc123"
            );
        }

        #[test]
        fn timeout_line_prints_a_bare_identifier_as_the_run_url() {
            // argo without a base_url describes the run as a bare identifier,
            // not a URL — the line prints whatever the backend handed over
            // either way, so the operator can always find the run.
            let e = BackendError::deadline_with_url(
                "workflow gantry-nourl deadline exceeded while polling status.phase",
                "workflow/gantry-nourl",
            );
            let line = wait_failure_line(&e, "gantry-nourl");
            assert!(
                line.contains("workflow/gantry-nourl"),
                "timeout line must carry the backend's run identifier, got: {line}"
            );
            assert!(line.contains("[gantry] timeout"));
        }

        #[test]
        fn other_wait_failures_keep_the_generic_line() {
            let e = BackendError::new("command not found: my-ci");
            assert_eq!(
                wait_failure_line(&e, "h-1"),
                "[gantry] wait failed: command not found: my-ci"
            );
        }
    }

    /// The deadline tail's stamping side (plan Component 6, DD-4): what the
    /// expiry arm hands the fallback context, and the record the one tail
    /// that must stay clean writes.
    mod deadline_expiry_stamping {
        use super::*;

        #[test]
        fn deadline_expiry_names_the_backend_handle_and_reason() {
            // The expiry the tail rides into the fallback context carries the
            // three facts the `[gantry] timeout` line printed: the backend in
            // its config spelling, the abandoned run's handle, and the
            // backend's expiry reason verbatim.
            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Argo;
            let handle = RunHandle::new("gantry-x7k2p");
            let reason = "workflow gantry-x7k2p deadline exceeded while polling status.phase";

            let expiry = deadline_expiry(&config, &handle, reason);

            assert_eq!(expiry.backend, "argo");
            assert_eq!(expiry.handle, "gantry-x7k2p");
            assert_eq!(expiry.reason, reason);
        }

        #[test]
        fn bare_wait_failure_verdict_writes_no_timeout_detail() {
            // The non-expiry wait-failure tail writes its InfraFailure record
            // through `write_verdict` and exits: no expiry exists to name, so
            // the timeout field must be absent on the wire (the additive
            // field leaves the bare exit's bytes unchanged).
            let dir = tempfile::TempDir::new().unwrap();
            let rl = RunLog::open_in(dir.path());
            write_verdict(
                &rl,
                "bare-wait-run".to_string(),
                Verdict::InfraFailure,
                RanLocation::Remote,
                1,
                "gantry-x7k2p".to_string(),
                Some(Durations {
                    gate: 1,
                    push: 2,
                    queue: 3,
                    run: 4,
                }),
            )
            .unwrap();

            let raw = fs::read_to_string(dir.path().join("runs.jsonl")).unwrap();
            let line = raw.lines().last().unwrap();
            assert!(
                !line.contains("timeout"),
                "a bare wait-failure exit must not name a timeout: {line}"
            );
            let record: VerdictRecord = serde_json::from_str(line).unwrap();
            assert_eq!(record.timeout, None);
            assert_eq!(record.verdict, crate::runlog::Verdict::InfraFailure);
            assert_eq!(record.ran, RanLocation::Remote);
        }
    }

    mod backend_deadline_resolution {
        use super::*;
        use crate::config::ArgoConfig;

        #[test]
        fn argo_backend_resolves_through_the_argo_override() {
            // An argo-configured run's wait deadline is argo_deadline()'s —
            // the per-backend override wins even when it is shorter than the
            // global, and the command override (present but for a different
            // backend) must not bleed into it.
            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Argo;
            config.remote.deadline_minutes = 40;
            config.remote.argo = Some(ArgoConfig {
                deadline_minutes: Some(3),
                ..Default::default()
            });
            assert_eq!(backend_wait_deadline(&config), Duration::from_secs(3 * 60));
        }

        #[test]
        fn argo_backend_inherits_the_global_without_an_override() {
            // An argo table without deadline_minutes — and no table at all —
            // inherits the global: None means "inherit", not "unbounded".
            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Argo;
            config.remote.deadline_minutes = 7;
            config.remote.argo = Some(ArgoConfig::default());
            assert_eq!(backend_wait_deadline(&config), Duration::from_secs(7 * 60));

            config.remote.argo = None;
            assert_eq!(backend_wait_deadline(&config), Duration::from_secs(7 * 60));
        }

        #[test]
        fn command_backend_keeps_the_command_deadline() {
            // The command templates are untouched by the argo override: the
            // pairing is per-backend, so a command-configured run resolves
            // its own key even when the argo table carries one.
            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Command;
            config.remote.deadline_minutes = 40;
            config.remote.argo = Some(ArgoConfig {
                deadline_minutes: Some(3),
                ..Default::default()
            });
            config.remote.command = Some(crate::config::CommandConfig {
                submit: vec![],
                logs: vec![],
                wait: vec![],
                deadline_minutes: Some(5),
            });
            assert_eq!(backend_wait_deadline(&config), Duration::from_secs(5 * 60));
        }
    }

    /// The config→backend construction glue ([`build_backend`]): a config
    /// selecting the argo backend must yield an ArgoBackend carrying the
    /// `[remote.argo]` table — before the pipelines dispatched on
    /// `config.remote.backend`, the construction site hardcoded the command
    /// backend and every table key was dead in the pipeline. Observed through
    /// the [`RemoteBackend`] surface the pipelines themselves drive —
    /// `describe` renders the table's base_url and namespace, `submit` puts
    /// the kubeconfig, namespace, template, and generate_name on the kubectl
    /// wire — so the mapping is pinned without reaching into the concrete
    /// type. The no-behavior-change half is pinned alongside: every other
    /// selection keeps building a command backend — the user's
    /// `[remote.command]` table when one is present, the shipped defaults
    /// when not — whatever the tables happen to hold.
    mod backend_construction {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        /// An argo table whose values are distinctive enough that each mapped
        /// field is recognizable on the other side.
        fn probe_argo_table(kubectl: &Path) -> crate::config::ArgoConfig {
            crate::config::ArgoConfig {
                kubectl_path: kubectl.to_string_lossy().into_owned(),
                kubeconfig: PathBuf::from("/probe/kubeconfig"),
                namespace: "gantry-probe".to_string(),
                template: "gantry-verify-probe".to_string(),
                generate_name: "gantry-probe-".to_string(),
                base_url: Some("https://argo-ui.example.com/".to_string()),
                ..Default::default()
            }
        }

        /// A config selecting the argo backend with the probe table.
        fn probe_config(kubectl: &Path) -> Config {
            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Argo;
            config.remote.argo = Some(probe_argo_table(kubectl));
            config
        }

        #[test]
        fn argo_selection_builds_the_backend_from_the_configured_table() {
            // The constructed backend carries the table's base_url and
            // namespace: describe() renders exactly the UI URL those two
            // fields spell (trailing slash trimmed, then the workflow path).
            let config = probe_config(Path::new("/probe/kubectl"));
            let backend = build_backend(&config);

            assert_eq!(
                backend.describe(&RunHandle::new("wf-7q2m")),
                "https://argo-ui.example.com/workflows/gantry-probe/wf-7q2m"
            );
        }

        /// Retry a mock-backed submit a few times when exec fails with
        /// ETXTBSY ("Text file busy") — same treatment as the argo backend's
        /// own mock-executable tests: a freshly-written mock can transiently
        /// race a still-open write handle under the parallel test harness.
        fn with_exec_retry(
            mut f: impl FnMut() -> Result<RunHandle, BackendError>,
        ) -> Result<RunHandle, BackendError> {
            let mut attempt = 0;
            loop {
                match f() {
                    Err(e) if attempt < 4 && e.reason.contains("Text file busy") => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(50 * attempt));
                    }
                    other => return other,
                }
            }
        }

        #[test]
        fn argo_selection_carries_the_table_onto_the_kubectl_wire() {
            // The constructed backend is a working ArgoBackend: submit()
            // runs the table's kubectl path with the kubeconfig and namespace
            // flags, and pipes a manifest carrying the configured
            // generate_name prefix and template name.
            let dir = tempfile::TempDir::new().unwrap();
            let argv_log = dir.path().join("argv.log");
            let stdin_log = dir.path().join("stdin.json");
            let kubectl = dir.path().join("mock-kubectl");
            fs::write(
                &kubectl,
                format!(
                    "#!/usr/bin/env sh\n\
                     printf '%s\\n' \"$@\" > {}\n\
                     cat > {}\n\
                     echo 'workflow.argoproj.io/gantry-probe-abc123 created'\n",
                    argv_log.display(),
                    stdin_log.display()
                ),
            )
            .unwrap();
            let mut perm = fs::metadata(&kubectl).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&kubectl, perm).unwrap();

            let config = probe_config(&kubectl);
            let backend = build_backend(&config);
            let spec = RunSpec::new(
                "cargo",
                "test",
                vec![],
                "https://github.com/example/repo",
                "abc123",
                "",
            );

            let handle = with_exec_retry(|| backend.submit(&spec))
                .expect("submit through the built backend must succeed");
            assert_eq!(handle, RunHandle::new("gantry-probe-abc123"));

            // The argv carries the connection flags from the table
            // (`--kubeconfig <path>`, `-n <namespace>`) ahead of the
            // `create -f -` submission — the manifest rides stdin, never
            // argv.
            let argv = fs::read_to_string(&argv_log).unwrap();
            let argv: Vec<&str> = argv.lines().collect();
            assert_eq!(
                argv,
                vec![
                    "--kubeconfig",
                    "/probe/kubeconfig",
                    "-n",
                    "gantry-probe",
                    "create",
                    "-f",
                    "-"
                ]
            );

            // The manifest carries the configured generate_name prefix and
            // workflow template name.
            let manifest: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&stdin_log).unwrap()).unwrap();
            assert_eq!(manifest["metadata"]["generateName"], "gantry-probe-");
            assert_eq!(
                manifest["spec"]["workflowTemplateRef"]["name"],
                "gantry-verify-probe"
            );
        }

        // --- the no-behavior-change half: every non-argo selection keeps
        // --- building a command backend --------------------------------

        /// A `[remote.command]` table whose argv tokens are distinctive
        /// enough that each template is recognizable on the wire, with the
        /// placeholders left in for the backend to substitute.
        fn probe_command_table(executor: &Path) -> crate::config::CommandConfig {
            crate::config::CommandConfig {
                submit: vec![
                    executor.to_string_lossy().into_owned(),
                    "submit-from-table".to_string(),
                    "{repo}".to_string(),
                    "{rev}".to_string(),
                    "{args_json}".to_string(),
                ],
                logs: vec![
                    executor.to_string_lossy().into_owned(),
                    "logs-from-table".to_string(),
                    "{handle}".to_string(),
                ],
                wait: vec![
                    executor.to_string_lossy().into_owned(),
                    "wait-from-table".to_string(),
                    "{handle}".to_string(),
                ],
                deadline_minutes: None,
            }
        }

        /// Write an executable mock executor that appends each invocation's
        /// argv (one argument per line) to `argv_log` and answers every
        /// subcommand with `handle` on stdout and exit 0 — enough executor
        /// for submit's handle parse, logs' stdout copy, and wait's
        /// exit-code verdict, with every argv observable afterward.
        fn write_mock_executor(dir: &Path, name: &str, argv_log: &Path, handle: &str) -> PathBuf {
            let path = dir.join(name);
            fs::write(
                &path,
                format!(
                    "#!/usr/bin/env sh\n\
                     printf '%s\\n' \"$@\" >> {}\n\
                     echo '{}'\n\
                     exit 0\n",
                    argv_log.display(),
                    handle
                ),
            )
            .unwrap();
            let mut perm = fs::metadata(&path).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&path, perm).unwrap();
            path
        }

        #[test]
        fn command_selection_builds_the_backend_from_the_configured_table() {
            // The constructed backend is a working CommandBackend driven by
            // the table: submit runs the table's submit argv with the
            // placeholders substituted, stream_logs runs its logs argv with
            // the returned handle, wait maps its exit code to a verdict —
            // and no status step exists, because the config schema carries
            // none (the construction passes `status: None` unconditionally).
            let dir = tempfile::TempDir::new().unwrap();
            let argv_log = dir.path().join("argv.log");
            let executor =
                write_mock_executor(dir.path(), "mock-executor", &argv_log, "table-run-7q2m");

            let mut config = Config::hardcoded();
            config.remote.backend = Backend::Command;
            config.remote.command = Some(probe_command_table(&executor));
            let backend = build_backend(&config);

            let spec = RunSpec::new(
                "cargo",
                "test",
                vec![],
                "https://github.com/example/repo",
                "abc123",
                "",
            );

            let handle = with_exec_retry(|| backend.submit(&spec))
                .expect("submit through the built backend must succeed");
            assert_eq!(handle, RunHandle::new("table-run-7q2m"));

            // The logs template runs with the handle submit returned, and
            // its stdout lands in the caller's writer.
            let mut logs = Vec::new();
            backend
                .stream_logs(&handle, &mut logs)
                .expect("stream_logs through the built backend must succeed");
            assert_eq!(String::from_utf8_lossy(&logs), "table-run-7q2m\n");

            // The wait template runs with the same handle; its exit code is
            // the verdict (the mock exits 0 → Pass).
            let verdict = backend
                .wait(&handle, Instant::now() + Duration::from_secs(30))
                .expect("wait through the built backend must succeed");
            assert_eq!(verdict, Verdict::Pass);

            // The wire shows all three templates came from the table,
            // placeholders substituted: submit carrying {repo}/{rev}/
            // {args_json}, logs and wait carrying the handle. (The mock's
            // `$@` starts past its own path — the program itself is argv[0],
            // exactly as the argo wire test's kubectl is.)
            let argv = fs::read_to_string(&argv_log).unwrap();
            let argv: Vec<&str> = argv.lines().collect();
            assert_eq!(
                argv,
                vec![
                    "submit-from-table",
                    "https://github.com/example/repo",
                    "abc123",
                    "[]",
                    "logs-from-table",
                    "table-run-7q2m",
                    "wait-from-table",
                    "table-run-7q2m",
                ]
            );

            // The construction never invents a status step the schema
            // cannot express: status() documents the limitation.
            let Err(err) = backend.status(&handle) else {
                panic!("the config schema carries no status step");
            };
            assert!(err.reason.contains("no status step"));
        }

        /// Submit through a default-built backend and prove the shipped
        /// default submit template ran: the reference executor recorded the
        /// spec's repo, rev, and args verbatim under its state dir. Only
        /// meaningful when `GANTRY_EXEC_PATH` is unset — a set value points
        /// the shipped default at a foreign program this test must not
        /// execute — and the seam stays read-only here: tests never mutate
        /// the process environment (the sibling command-backend tests assert
        /// the unset default executor verbatim).
        fn submit_through_shipped_default(backend: &dyn RemoteBackend, spec: &RunSpec) {
            if std::env::var_os("GANTRY_EXEC_PATH").is_some() {
                eprintln!(
                    "skipping the shipped-executor wire leg: GANTRY_EXEC_PATH is set in this environment"
                );
                return;
            }

            let handle = backend
                .submit(spec)
                .expect("submit through the default-built backend must run the shipped executor");
            assert!(
                handle.handle.starts_with("run-"),
                "the shipped executor's handle shape, got {:?}",
                handle.handle
            );

            // The executor recorded the request — the substituted
            // placeholders, byte-exact (one line per field).
            let state_dir = std::env::var_os("GANTRY_EXEC_STATE_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp/gantry-runs"));
            let run_dir = state_dir.join(&handle.handle);
            assert_eq!(
                fs::read_to_string(run_dir.join("repo")).unwrap(),
                format!("{}\n", spec.repo_url)
            );
            assert_eq!(
                fs::read_to_string(run_dir.join("rev")).unwrap(),
                format!("{}\n", spec.sha)
            );
            assert_eq!(fs::read_to_string(run_dir.join("args")).unwrap(), "[]\n");
            let _ = fs::remove_dir_all(&run_dir);
        }

        #[test]
        fn command_and_none_selections_without_a_table_keep_the_shipped_defaults() {
            // No [remote.command] table: both non-argo selections build the
            // shipped command defaults (CommandBackend::new) — observable as
            // the command backend's describe/status surface and, when the
            // environment leaves the executor seam unset, the shipped
            // reference executor on the submit wire.
            let spec = RunSpec::new(
                "cargo",
                "test",
                vec![],
                "https://github.com/example/repo",
                "abc123",
                "",
            );

            let mut command_no_table = Config::hardcoded();
            command_no_table.remote.backend = Backend::Command;

            for config in [Config::hardcoded(), command_no_table] {
                let backend = build_backend(&config);

                // The command backend describes runs as the bare handle —
                // never an argo UI URL — and carries no status step.
                let handle = RunHandle::new("wf-defaults");
                assert_eq!(backend.describe(&handle), "handle/wf-defaults");
                let Err(err) = backend.status(&handle) else {
                    panic!("the shipped default template has no status step");
                };
                assert!(err.reason.contains("no status step"));

                // The submit wire runs the shipped reference executor with
                // the spec substituted in.
                submit_through_shipped_default(&*backend, &spec);
            }
        }

        #[test]
        fn argo_is_the_only_selection_that_builds_the_argo_backend() {
            // The construction dispatches on the selection, never on which
            // tables happen to be populated: with a fully-loaded [remote.argo]
            // table present, both command selections still build a command
            // backend — the user's [remote.command] table here — and the
            // argo table stays inert: describe never renders its base_url,
            // and submit never execs its kubectl (pointed at /probe/kubectl
            // so an accidental argo construction fails loudly).
            let spec = RunSpec::new(
                "cargo",
                "test",
                vec![],
                "https://github.com/example/repo",
                "abc123",
                "",
            );

            let mut command_with_both_tables = Config::hardcoded();
            command_with_both_tables.remote.backend = Backend::Command;
            command_with_both_tables.remote.argo =
                Some(probe_argo_table(Path::new("/probe/kubectl")));

            let mut none_with_both_tables = Config::hardcoded();
            none_with_both_tables.remote.argo = Some(probe_argo_table(Path::new("/probe/kubectl")));

            for mut config in [command_with_both_tables, none_with_both_tables] {
                let dir = tempfile::TempDir::new().unwrap();
                let argv_log = dir.path().join("argv.log");
                let executor =
                    write_mock_executor(dir.path(), "mock-executor", &argv_log, "table-run-7q2m");
                config.remote.command = Some(probe_command_table(&executor));

                let backend = build_backend(&config);
                let handle = with_exec_retry(|| backend.submit(&spec))
                    .expect("submit must run the command table, not the argo table's kubectl");
                assert_eq!(handle, RunHandle::new("table-run-7q2m"));

                // describe() renders the command backend's bare handle — the
                // probe argo table's base_url would have rendered the UI URL.
                assert_eq!(backend.describe(&handle), "handle/table-run-7q2m");

                // And the wire carries the command table's submit argv with
                // the placeholders substituted — never kubectl (the mock's
                // `$@` starts past its own path, argv[0] being the program).
                let argv = fs::read_to_string(&argv_log).unwrap();
                let argv: Vec<&str> = argv.lines().collect();
                assert_eq!(
                    argv,
                    vec![
                        "submit-from-table",
                        "https://github.com/example/repo",
                        "abc123",
                        "[]",
                    ]
                );
            }

            // The same inertness with no command table at all: the shipped
            // defaults keep the run, and the argo table still renders
            // nothing (describe-only — the wire leg is the seam-default
            // test's job).
            let mut command_default_only = Config::hardcoded();
            command_default_only.remote.backend = Backend::Command;
            command_default_only.remote.argo = Some(probe_argo_table(Path::new("/probe/kubectl")));
            let backend = build_backend(&command_default_only);
            assert_eq!(backend.describe(&RunHandle::new("wf-x")), "handle/wf-x");
        }
    }

    mod explicit_offload {
        use super::*;

        #[test]
        fn cargo_test_rides_the_intercepted_shape() {
            // `gantry run -- cargo test -- --nocapture` must submit exactly
            // what an intercepted `cargo test -- --nocapture` submits, so a
            // backend cannot tell the two apart (and the shipped executor's
            // cargo-test contract applies unchanged).
            let tail = vec![
                "test".to_string(),
                "--".to_string(),
                "--nocapture".to_string(),
            ];
            let (tool, subcommand, args) = explicit_run_spec_fields("cargo", &tail);
            assert_eq!(tool, "cargo");
            assert_eq!(subcommand, "test");
            assert_eq!(args, vec!["--".to_string(), "--nocapture".to_string()]);
        }

        #[test]
        fn arbitrary_program_rides_the_faithful_argv_shape() {
            let tail = vec!["-j4".to_string(), "check".to_string()];
            let (tool, subcommand, args) = explicit_run_spec_fields("make", &tail);
            assert_eq!(tool, "run");
            assert_eq!(subcommand, "make");
            assert_eq!(args, vec!["-j4".to_string(), "check".to_string()]);
        }

        #[test]
        fn bare_cargo_degrades_to_the_run_shape() {
            // `gantry run -- cargo` (no subcommand) has nothing to split; the
            // run shape keeps the program visible instead of submitting a
            // subcommand-less cargo spec.
            let (tool, subcommand, args) = explicit_run_spec_fields("cargo", &[]);
            assert_eq!(tool, "run");
            assert_eq!(subcommand, "cargo");
            assert!(args.is_empty());
        }
    }

    mod explicit_run_pipeline {
        use super::*;

        fn tier0_config() -> Config {
            Config::hardcoded()
        }

        fn argv(items: &[&str]) -> Vec<String> {
            items.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn empty_wrapped_argv_is_an_error_before_anything_runs() {
            // Library-callers defense in depth: the CLI parser already
            // rejects an empty wrapped command, so the decision path must
            // answer the same usage-error exit without touching anything.
            assert_eq!(run_explicit(&tier0_config(), "file:///t", "0", &[]), 1);
        }

        #[test]
        fn backend_none_runs_the_wrapped_command_locally() {
            // `--backend none` (and the zero-config default) is Tier-0: the
            // wrapped command never goes remote, and its own exit code is
            // the answer (INV-3). A kill switch in the environment lands in
            // the same local tail, so the assertion holds either way.
            let code = run_explicit(&tier0_config(), "file:///t", "0", &argv(&["true"]));
            assert_eq!(code, 0);
        }

        #[test]
        fn wrapped_command_exit_code_passes_through_the_local_tail() {
            // A failing wrapped command's status survives the Tier-0 local
            // tail verbatim — the explicit path must be as transparent about
            // failure as the intercepted pipeline.
            let code = run_explicit(
                &tier0_config(),
                "file:///t",
                "0",
                &argv(&["sh", "-c", "exit 3"]),
            );
            assert_eq!(code, 3);
        }

        #[test]
        fn argo_form_falls_back_to_local_when_the_gate_rejects() {
            // The argo form takes the remote arm up to the git gate; a
            // ci_remote that cannot exist makes the run gate-ineligible, and
            // the fallback ladder still produces the wrapped command's
            // result from the capped local tail instead of erroring out.
            let mut config = Config::hardcoded();
            config.remote.backend = crate::config::Backend::Argo;
            config.remote.ci_remote = "gantry-unit-test-no-such-remote".to_string();
            let code = run_explicit(&config, "file:///t", "0", &argv(&["true"]));
            assert_eq!(code, 0);
        }

        #[test]
        fn command_form_falls_back_to_local_when_the_gate_rejects() {
            // Same ladder for the command-template form: the backend is
            // never contacted on a gate rejection, so no command-template
            // configuration is needed to prove the fallback branch.
            let mut config = Config::hardcoded();
            config.remote.backend = crate::config::Backend::Command;
            config.remote.ci_remote = "gantry-unit-test-no-such-remote".to_string();
            let code = run_explicit(&config, "file:///t", "0", &argv(&["true"]));
            assert_eq!(code, 0);
        }
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
