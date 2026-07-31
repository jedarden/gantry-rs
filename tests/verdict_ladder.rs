// gantry — the verdict ladder end to end (bf-4rx9).
//
// The unit tests in `backend` and `decision` each cover one half of the ladder:
// a backend status becomes a [`Verdict`], and a [`Verdict`] becomes a
// [`RetryDecision`]. This file joins them across the public API, driving a real
// executor script through `submit` → `wait` → `decide_retry` so the property the
// plan actually cares about is asserted on the whole path rather than on either
// half:
//
//   - an infra failure (and only an infra failure) reaches a local run (DD-4),
//   - a test or gate failure never does, whatever the fallback budget (INV-7),
//   - and every rung's exit code survives to the caller (INV-3).
//
// The executor is the same shape as `contrib/gantry-exec.sh`: a script whose
// `wait` prints a verdict.json (or not) and exits. That is the real status
// surface, so these are contract tests, not mock theater.

use gantry::backend::command::{CommandBackend, CommandConfig};
use gantry::backend::{RemoteBackend, RunHandle, RunSpec, Verdict};
use gantry::decision::{decide_retry, FallbackPolicy, RetryDecision};

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// An executor whose `wait` behavior is keyed off the handle it is given.
///
/// One keyword per rung of the ladder:
///
/// | handle    | exit | stdout                          | verdict            |
/// |-----------|------|---------------------------------|--------------------|
/// | `pass`    | 0    | —                               | Pass               |
/// | `fail`    | 1    | —                               | TestFailure(1)     |
/// | `boom`    | 3    | —                               | InfraFailure       |
/// | `oom`     | 1    | verdict.json `{oom:true}`       | InfraFailure       |
/// | `deadline`| 1    | verdict.json `{deadline...}`    | InfraFailure       |
/// | `clippy`  | 1    | verdict.json `{gate_failed}`    | GateFailure(clippy)|
/// | `compile` | 1    | verdict.json `{failure_class}`  | TestFailure(101)   |
/// | `future`  | 1    | verdict.json schema_version 99  | InfraFailure       |
fn write_executor(dir: &Path) -> PathBuf {
    let path = dir.join("ladder-exec.sh");
    let mut file = std::fs::File::create(&path).expect("create executor");
    file.write_all(
        br#"#!/usr/bin/env bash
case "$1" in
    submit) echo "handle-$3"; exit 0 ;;
    wait)
        case "$2" in
            *oom*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":137,"oom":true}'; exit 1 ;;
            *deadline*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":1,"deadline_exceeded":true}'; exit 1 ;;
            *clippy*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":101,"gate_failed":"clippy"}'; exit 1 ;;
            *compile*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":101,"failure_class":"compile_error"}'; exit 1 ;;
            *future*)
                echo '{"schema_version":99,"phase":"Succeeded","exit_code":0}'; exit 1 ;;
            *boom*) exit 3 ;;
            *pass*) exit 0 ;;
            *)      exit 1 ;;
        esac ;;
    *) echo "unknown: $1" >&2; exit 2 ;;
esac
"#,
    )
    .expect("write executor");
    path
}

/// A backend wired to the executor above.
///
/// Invoked as `bash <script>`: exec'ing a file another thread may still hold open
/// for writing fails with ETXTBSY, and reading it costs nothing here.
fn backend(executor: &Path) -> CommandBackend {
    let exe = executor.to_str().expect("temp path is utf-8").to_string();
    CommandBackend::new(CommandConfig {
        submit: vec![
            "bash".to_string(),
            exe.clone(),
            "submit".to_string(),
            "{repo}".to_string(),
            "{rev}".to_string(),
        ],
        wait: vec![
            "bash".to_string(),
            exe,
            "wait".to_string(),
            "{handle}".to_string(),
        ],
        logs: None,
        cancel: None,
    })
}

fn spec(sha: &str) -> RunSpec {
    RunSpec::new(
        "cargo",
        "test",
        Vec::new(),
        "file:///repo",
        sha,
        PathBuf::from("."),
    )
}

/// Drive one run all the way through: submit → wait → the ladder.
fn run(executor: &Path, sha: &str, policy: &FallbackPolicy) -> (Verdict, RetryDecision) {
    let backend = backend(executor);
    let handle = backend.submit(&spec(sha)).expect("submit should succeed");
    assert!(
        matches!(&handle, RunHandle::Command { handle } if handle.contains(sha)),
        "the handle must be the executor's stdout, got {handle:?}",
    );

    let verdict = backend
        .wait(&handle, Instant::now())
        .expect("wait should run");
    let decision = decide_retry(&verdict, 0, policy);
    (verdict, decision)
}

#[test]
fn a_passing_run_ladders_to_exit_zero() {
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());

    let (verdict, decision) = run(&exec, "pass", &FallbackPolicy::capped());
    assert_eq!(verdict, Verdict::Pass);
    assert_eq!(decision, RetryDecision::Terminal { exit_code: 0 });
}

#[test]
fn a_test_failure_ladders_to_a_terminal_non_zero_exit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());

    // Both shapes of "the suite failed": the bare exit-code contract, and a
    // verdict.json carrying a failure class with its own exit code.
    let (verdict, decision) = run(&exec, "fail", &FallbackPolicy::capped());
    assert_eq!(verdict, Verdict::test_failure(1));
    assert_eq!(decision, RetryDecision::Terminal { exit_code: 1 });

    let (verdict, decision) = run(&exec, "compile", &FallbackPolicy::capped());
    assert_eq!(verdict, Verdict::test_failure(101));
    assert_eq!(
        decision,
        RetryDecision::Terminal { exit_code: 101 },
        "INV-3: the remote's exit code must reach the caller unchanged",
    );
}

#[test]
fn a_gate_failure_ladders_to_a_terminal_exit_naming_the_gate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());

    let (verdict, decision) = run(&exec, "clippy", &FallbackPolicy::capped());
    assert_eq!(verdict, Verdict::gate_failure("clippy".to_string(), 101));
    assert_eq!(decision, RetryDecision::Terminal { exit_code: 101 });
}

#[test]
fn every_infra_shape_ladders_to_a_capped_local_fallback() {
    // DD-4: no verdict was produced about the code — not by a scheduling failure,
    // not by an OOM kill, not by a deadline, not by a verdict.json this client
    // cannot read — so the only honest answer left is a local run.
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());

    for sha in ["boom", "oom", "deadline", "future"] {
        let (verdict, decision) = run(&exec, sha, &FallbackPolicy::capped());
        assert!(
            matches!(verdict, Verdict::InfraFailure { .. }),
            "{sha}: expected InfraFailure, got {verdict}",
        );
        assert!(
            matches!(decision, RetryDecision::LocalFallback { attempt: 1, .. }),
            "{sha}: an infra failure must buy exactly one local run, got {decision:?}",
        );
    }
}

#[test]
fn an_infra_failure_with_the_cap_spent_is_loud_and_non_zero() {
    // A backend outage with no fallback budget left may not quietly exit 0: an
    // absent verdict is never a pass.
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());

    let (_, decision) = run(&exec, "boom", &FallbackPolicy::no_fallback());
    match decision {
        RetryDecision::FallbackExhausted { reason, exit_code } => {
            assert!(!reason.is_empty(), "the infra reason must survive");
            assert_ne!(exit_code, 0);
        }
        other => panic!("expected FallbackExhausted, got {other:?}"),
    }
}

#[test]
fn inv7_no_budget_turns_a_real_failure_into_a_local_rerun() {
    // The invariant over the whole (rung × budget) space, driven from the backend
    // status rather than from a hand-built Verdict: whatever the fallback cap and
    // however many fallbacks were already spent, a real failure stays terminal.
    let dir = tempfile::tempdir().expect("temp dir");
    let exec = write_executor(dir.path());
    let backend = backend(&exec);

    for sha in ["fail", "compile", "clippy"] {
        let handle = backend.submit(&spec(sha)).expect("submit should succeed");
        let verdict = backend
            .wait(&handle, Instant::now())
            .expect("wait should run");
        assert!(
            !verdict.should_fallback(),
            "{sha}: {verdict} must not fall back"
        );

        for used in 0..3u32 {
            for cap in 0..3u32 {
                let decision = decide_retry(&verdict, used, &FallbackPolicy::with_cap(cap));
                assert!(
                    matches!(decision, RetryDecision::Terminal { exit_code } if exit_code != 0),
                    "INV-7: {verdict} must stay terminal and red (used={used}, cap={cap}), got {decision:?}",
                );
            }
        }
    }
}
