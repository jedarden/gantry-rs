// gantry — integration tests for the Argo backend's log streaming and status
// polling (bf-6c1v).
//
// Drives ArgoBackend end-to-end against a stateful mock kubectl that simulates
// the pod lifecycle the backend must tolerate:
//
//   workflow pending (no pod, no status) -> pod scheduled
//   -> `kubectl logs -f` streams the run -> pod terminates
//
// plus the podGC: OnPodCompletion twist — the pod is deleted the moment the
// run finishes, so a fast run goes straight from "no pod" to a terminal
// workflow whose log exists only as the `output` output parameter. The mock
// walks through these states via a per-subcommand call counter baked into its
// script, and the tests assert on both the streamed bytes and the argv the
// mock actually received.
//
// The mock dispatches on the whole argv (`case " $* " in`): ArgoBackend
// prepends `-n <namespace>` before the subcommand, so positional checks see
// `-n`, never `get`/`logs` as $1. Each test gets its own temp dir and bakes
// paths into its mock — no shared state, no env vars — so the suite stays
// hermetic under parallel `cargo test` (same idiom as
// tests/command_backend_integration.rs).

use gantry::backend::argo::{ArgoBackend, ArgoConfig};
use gantry::backend::{BackendError, RemoteBackend, RunHandle, Verdict};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Write an executable mock kubectl into `dir` and return its path.
fn write_mock_kubectl(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("mock-kubectl");
    let mut file = fs::File::create(&path).expect("create mock kubectl");
    file.write_all(body.as_bytes()).expect("write mock kubectl");
    let mut perm = fs::metadata(&path)
        .expect("stat mock kubectl")
        .permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm).expect("make mock kubectl executable");
    path
}

fn kubectl_path(p: &Path) -> String {
    p.to_str().expect("temp path is valid utf-8").to_string()
}

/// Retry a mock-backed call a few times when exec fails with ETXTBSY
/// ("Text file busy"). Under the parallel test harness, exec of a
/// freshly-written mock can transiently race a still-open write handle from
/// another test's fork/exec traffic; the condition clears once every
/// straggler handle closes. Same treatment as the argo unit tests and
/// tests/command_backend_integration.rs. Production exec paths deliberately
/// do NOT get this — they must surface real spawn errors loudly.
fn with_exec_retry<T>(mut f: impl FnMut() -> Result<T, BackendError>) -> Result<T, BackendError> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e) if attempt < 4 && e.reason.contains("Text file busy") => {
                attempt += 1;
                thread::sleep(Duration::from_millis(50 * attempt));
            }
            other => return other,
        }
    }
}

/// A backend pointed at a mock kubectl in `dir` with default Argo settings.
fn backend_with(kubectl: &Path) -> ArgoBackend {
    ArgoBackend::new(ArgoConfig {
        kubectl_path: kubectl_path(kubectl),
        ..ArgoConfig::default()
    })
}

/// The terminal workflow object the mock serves, with an optional `output`
/// output parameter carrying the recovered log.
fn terminal_workflow_json(output_param: Option<&str>) -> String {
    let output = match output_param {
        Some(log) => format!(
            r#", "outputs": {{"parameters": [{{"name": "output", "value": {}}}]}}"#,
            serde_json::to_string(log).expect("log is valid JSON string")
        ),
        None => String::new(),
    };
    format!(
        r#"{{"apiVersion":"argoproj.io/v1alpha1","kind":"Workflow",
            "metadata":{{"name":"gantry-abc123"}},
            "status":{{"phase":"Succeeded"{}}}}}"#,
        output
    )
}

/// Happy-path pod lifecycle: the workflow starts pending (pod not yet
/// scheduled), the pod appears on the next poll, and `kubectl logs -f`
/// streams the run output. Asserts the streamed bytes and that streaming
/// really went through `logs -f <pod>`.
#[test]
fn logs_stream_from_pod_across_lifecycle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let logs_argv = tmp.path().join("logs-argv.txt");
    let pod_calls = tmp.path().join("pod-calls");
    fs::write(&pod_calls, "0").expect("seed pod call counter");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*)\n\
                 n=$(($(cat {pod_calls}) + 1)); echo $n > {pod_calls}\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                   echo '{{\"items\":[]}}'\n\
                 else\n\
                   echo '{{\"items\":[{{\"metadata\":{{\"name\":\"gantry-abc123-1234567890\"}}}}]}}'\n\
                 fi ;;\n\
               *' logs '*)\n\
                 printf '%s\\n' \"$@\" > '{logs_argv}'\n\
                 printf 'running 137 tests\\nall passed\\n' ;;\n\
               *' get workflow '*) echo '{{\"status\":{{\"phase\":\"Running\"}}}}' ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            pod_calls = pod_calls.display(),
            logs_argv = logs_argv.display(),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let mut out: Vec<u8> = Vec::new();
    with_exec_retry(|| backend.stream_logs(&handle, &mut out))
        .expect("stream_logs must survive a pending phase and stream once the pod appears");
    assert_eq!(
        String::from_utf8_lossy(&out),
        "running 137 tests\nall passed\n"
    );

    // The stream must come from a follow on the discovered pod, not a
    // one-shot fetch.
    let argv = fs::read_to_string(&logs_argv).expect("mock recorded logs argv");
    let argv: Vec<&str> = argv.lines().collect();
    assert!(
        argv.contains(&"-f"),
        "logs must use -f follow mode, got: {:?}",
        argv
    );
    assert!(
        argv.contains(&"gantry-abc123-1234567890"),
        "logs must target the discovered pod, got: {:?}",
        argv
    );
}

/// podGC'd fast run: the pod is deleted at completion, so discovery never
/// sees one — but the workflow is already terminal, so stream_logs must skip
/// the full discovery timeout and recover the full log from the `output`
/// output parameter instead.
#[test]
fn podgc_fast_run_recovers_full_log_from_output_parameter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{{\"items\":[]}}' ;;\n\
               *' get workflow '*) cat <<'JSON'\n{}\nJSON\n ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            terminal_workflow_json(Some("compile exited 0\n137 passed\n")),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let start = Instant::now();
    let mut out: Vec<u8> = Vec::new();
    with_exec_retry(|| backend.stream_logs(&handle, &mut out))
        .expect("podGC'd run must recover its log from the output parameter");
    // Terminal on the first poll: no discovery-timeout hang, no poll sleep.
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "podGC recovery must not wait out the discovery timeout, took {:?}",
        start.elapsed()
    );
    assert_eq!(
        String::from_utf8_lossy(&out),
        "compile exited 0\n137 passed\n"
    );
}

/// The pod appears but vanishes (podGC) before `kubectl logs -f` can attach:
/// the failing stream falls back to the output parameter rather than
/// failing the run.
#[test]
fn pod_vanishing_before_logs_falls_back_to_output_parameter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{{\"items\":[{{\"metadata\":{{\"name\":\"gantry-abc123-1234567890\"}}}}]}}' ;;\n\
               *' logs '*) echo 'Error from server (NotFound): pod not found' >&2; exit 1 ;;\n\
               *' get workflow '*) cat <<'JSON'\n{}\nJSON\n ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            terminal_workflow_json(Some("recovered after pod vanished\n")),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let mut out: Vec<u8> = Vec::new();
    with_exec_retry(|| backend.stream_logs(&handle, &mut out))
        .expect("a vanished pod must fall back to the output parameter");
    assert_eq!(
        String::from_utf8_lossy(&out),
        "recovered after pod vanished\n"
    );
}

/// The pod vanishes (podGC) mid-stream: `kubectl logs -f` has already piped a
/// prefix of the run into the writer before dying with the pod. The recovery
/// appends the full log from the `output` output parameter, so no bytes are
/// lost — the reader sees the partial stream followed by the complete log.
#[test]
fn pod_vanishing_mid_stream_appends_full_recovered_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{{\"items\":[{{\"metadata\":{{\"name\":\"gantry-abc123-1234567890\"}}}}]}}' ;;\n\
               *' logs '*) printf 'running 137 tests\\ntest backend::tests ... '; exit 1 ;;\n\
               *' get workflow '*) cat <<'JSON'\n{}\nJSON\n ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            terminal_workflow_json(Some("running 137 tests\nall 137 passed\n")),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let mut out: Vec<u8> = Vec::new();
    with_exec_retry(|| backend.stream_logs(&handle, &mut out))
        .expect("a pod dying mid-stream must fall back to the output parameter");
    assert_eq!(
        String::from_utf8_lossy(&out),
        // Partial stream, then the full recovered log: recovery appends, so
        // the mid-stream prefix is preserved rather than overwritten.
        "running 137 tests\ntest backend::tests ... running 137 tests\nall 137 passed\n",
    );
}

/// Nothing to recover: the pod is gone and the workflow carries no `output`
/// parameter — streaming fails loudly instead of reporting silent success.
#[test]
fn podgc_without_output_parameter_is_a_loud_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{{\"items\":[]}}' ;;\n\
               *' get workflow '*) cat <<'JSON'\n{}\nJSON\n ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            terminal_workflow_json(None),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let mut out: Vec<u8> = Vec::new();
    let err = with_exec_retry(|| backend.stream_logs(&handle, &mut out))
        .expect_err("no pod and no output parameter must be a loud error");
    assert!(
        err.reason.contains("no logs available") && err.reason.contains("gantry-abc123"),
        "{}",
        err.reason
    );
    assert!(out.is_empty(), "nothing should be written on failure");
}

/// Status polling: wait() walks the workflow from unreconciled (no status
/// stanza) through Running to Succeeded, reads the `verdict` output
/// parameter, and reports Pass — the authoritative verdict path.
#[test]
fn wait_polls_through_lifecycle_to_terminal_verdict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wf_calls = tmp.path().join("wf-calls");
    fs::write(&wf_calls, "0").expect("seed workflow call counter");
    let verdict_json = r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0,
         "oom": false, "deadline_exceeded": false}"#;
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' get workflow '*)\n\
                 n=$(($(cat {wf_calls}) + 1)); echo $n > {wf_calls}\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                   echo '{{\"metadata\":{{\"name\":\"gantry-abc123\"}}}}'\n\
                 elif [ \"$n\" -eq 2 ]; then\n\
                   echo '{{\"status\":{{\"phase\":\"Running\"}}}}'\n\
                 else\n\
                   cat <<'JSON'\n{{\"status\":{{\"phase\":\"Succeeded\",\"outputs\":{{\"parameters\":[{{\"name\":\"verdict\",\"value\":{verdict_json}}}]}}}}}}\nJSON\n\
                 fi ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            wf_calls = wf_calls.display(),
            verdict_json = serde_json::to_string(verdict_json).expect("verdict json string"),
        ),
    );
    let backend = backend_with(&kubectl);
    let handle = RunHandle::new("gantry-abc123");

    let verdict =
        with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(60)))
            .expect("wait must poll through pending/Running to terminal");
    assert_eq!(verdict, Verdict::Pass);

    // All three lifecycle states must actually have been served.
    let calls: u32 = fs::read_to_string(&wf_calls)
        .expect("workflow call counter")
        .trim()
        .parse()
        .expect("counter is a number");
    assert!(
        calls >= 3,
        "wait must have polled the lifecycle, got {} calls",
        calls
    );
}
