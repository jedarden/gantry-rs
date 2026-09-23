// gantry — integration tests for the command-template backend (bf-3rer).
//
// Drives CommandBackend end-to-end against mock CI commands (real executables
// written to a per-test temp dir): submit captures stdout as the handle,
// wait maps the mock's exit code onto the verdict ladder (0 -> Pass,
// 1 -> TestFailure, >=2 -> InfraFailure), and the logs argv streams through to
// a writer. The mocks record the argv they were invoked with, so the tests
// also prove {repo}, {rev}, {args_json} and {handle} are substituted at the
// argv level before the command runs.
//
// Each test gets its own temp dir and bakes paths into its mock scripts —
// no shared state, no env vars — so the suite stays hermetic under parallel
// `cargo test` (gantry-275ec80c).

use gantry::backend::command::{CommandBackend, CommandConfig};
use gantry::backend::{RemoteBackend, RunHandle, RunSpec, Verdict};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Write an executable bash script into `dir` and return its path.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = fs::File::create(&path).expect("create mock script");
    file.write_all(body.as_bytes()).expect("write mock script");
    #[cfg(unix)]
    {
        let mut perm = fs::metadata(&path).expect("stat mock script").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&path, perm).expect("make mock script executable");
    }
    path
}

fn script_path(p: &Path) -> String {
    p.to_str().expect("temp path is valid utf-8").to_string()
}

/// Mock submit: records its argv (one per line) and emits a handle on stdout.
fn write_submit_mock(dir: &Path, handle: &str) -> PathBuf {
    write_script(
        dir,
        "mock-submit",
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"{}/submit-argv.txt\"\necho '{}'\n",
            dir.display(),
            handle
        ),
    )
}

/// Mock submit that fails with exit 3 and a stderr message.
fn write_failing_submit_mock(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "mock-submit-fail",
        "#!/usr/bin/env bash\necho 'mock submit exploded' >&2\nexit 3\n",
    )
}

/// Mock submit that exits 0 without emitting any stdout (no handle).
fn write_empty_submit_mock(dir: &Path) -> PathBuf {
    write_script(dir, "mock-submit-empty", "#!/usr/bin/env bash\nexit 0\n")
}

/// Mock wait: records its argv, then exits with the baked-in code.
fn write_wait_mock(dir: &Path, exit_code: i32) -> PathBuf {
    write_script(
        dir,
        &format!("mock-wait-{}", exit_code),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"{}/wait-argv-{}.txt\"\nexit {}\n",
            dir.display(),
            exit_code,
            exit_code
        ),
    )
}

/// Mock logs: echoes two lines, the first carrying the handle argument.
fn write_logs_mock(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "mock-logs",
        "#!/usr/bin/env bash\necho \"log line for $1\"\necho 'second line'\n",
    )
}

/// Build a backend whose argv templates point at the given mock scripts with
/// one placeholder each, so substitution and invocation are both observable.
fn backend_with(submit: PathBuf, wait: PathBuf, logs: PathBuf) -> CommandBackend {
    CommandBackend::with_config(CommandConfig {
        submit: vec![
            script_path(&submit),
            "{repo}".to_string(),
            "{rev}".to_string(),
            "{args_json}".to_string(),
        ],
        logs: vec![script_path(&logs), "{handle}".to_string()],
        wait: vec![script_path(&wait), "{handle}".to_string()],
    })
}

fn spec() -> RunSpec {
    RunSpec::new(
        "cargo",
        "test",
        vec![
            "test".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ],
        "file:///repos/demo",
        "abc123",
        "",
    )
}

/// Read back an argv record written by a mock (one argument per line).
fn read_argv_record(dir: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(dir.join(name))
        .expect("mock argv record should exist")
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn submit_captures_stdout_as_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1234"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let handle = backend.submit(&spec()).expect("submit should succeed");
    assert_eq!(handle.handle, "run-1234");
}

#[test]
fn submit_substitutes_placeholders_at_argv_level() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    backend.submit(&spec()).expect("submit should succeed");

    // "$@" excludes $0, so the record is exactly the three placeholder slots.
    let argv = read_argv_record(dir.path(), "submit-argv.txt");
    assert_eq!(
        argv,
        vec![
            "file:///repos/demo".to_string(),
            "abc123".to_string(),
            // serde_json encoding: one argument, no spaces, JSON-quoted.
            "[\"test\",\"--\",\"--nocapture\"]".to_string(),
        ],
        "each placeholder must become exactly one argv element"
    );
}

#[test]
fn submit_nonzero_exit_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_failing_submit_mock(dir.path()),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let err = backend.submit(&spec()).expect_err("submit should fail");
    assert!(
        err.reason.contains("submit command failed"),
        "error should attribute the failure to submit, got: {}",
        err.reason
    );
    assert!(
        err.reason.contains("mock submit exploded"),
        "stderr should surface in the error, got: {}",
        err.reason
    );
}

#[test]
fn submit_empty_stdout_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_empty_submit_mock(dir.path()),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let err = backend
        .submit(&spec())
        .expect_err("empty handle should fail");
    assert!(err.reason.contains("empty handle"), "got: {}", err.reason);
}

#[test]
fn wait_exit_0_maps_to_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let verdict = backend
        .wait(&RunHandle::new("run-1"), Instant::now())
        .expect("wait should succeed");
    assert_eq!(verdict, Verdict::Pass);
}

#[test]
fn wait_exit_1_maps_to_test_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1"),
        write_wait_mock(dir.path(), 1),
        write_logs_mock(dir.path()),
    );

    let verdict = backend
        .wait(&RunHandle::new("run-1"), Instant::now())
        .expect("wait should succeed");
    assert_eq!(verdict, Verdict::TestFailure);
}

#[test]
fn wait_exit_2_and_higher_map_to_infra_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let logs = write_logs_mock(dir.path());
    let submit = write_submit_mock(dir.path(), "run-1");

    for code in [2, 42, 127] {
        let backend = backend_with(
            submit.clone(),
            write_wait_mock(dir.path(), code),
            logs.clone(),
        );
        let verdict = backend
            .wait(&RunHandle::new("run-1"), Instant::now())
            .expect("wait should succeed");
        assert_eq!(verdict, Verdict::InfraFailure, "exit {}", code);
    }
}

#[test]
fn wait_receives_substituted_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    backend
        .wait(&RunHandle::new("run-77"), Instant::now())
        .expect("wait should succeed");

    let argv = read_argv_record(dir.path(), "wait-argv-0.txt");
    assert_eq!(
        argv,
        vec!["run-77"],
        "{{handle}} must become one argv element"
    );
}

#[test]
fn logs_streams_command_stdout_to_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-1"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let mut out: Vec<u8> = Vec::new();
    backend
        .stream_logs(&RunHandle::new("run-9"), &mut out)
        .expect("stream_logs should succeed");

    let text = String::from_utf8(out).expect("log output is utf-8");
    assert!(
        text.contains("log line for run-9"),
        "logs should carry the substituted handle, got: {}",
        text
    );
    assert!(
        text.contains("second line"),
        "all stdout lines should stream through, got: {}",
        text
    );
}

#[test]
fn round_trip_submit_then_wait_passes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-ok"),
        write_wait_mock(dir.path(), 0),
        write_logs_mock(dir.path()),
    );

    let handle = backend.submit(&spec()).expect("submit should succeed");
    let verdict = backend
        .wait(&handle, Instant::now())
        .expect("wait should succeed");
    assert_eq!(verdict, Verdict::Pass);
}

#[test]
fn round_trip_submit_then_wait_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = backend_with(
        write_submit_mock(dir.path(), "run-bad"),
        write_wait_mock(dir.path(), 1),
        write_logs_mock(dir.path()),
    );

    let handle = backend.submit(&spec()).expect("submit should succeed");
    let verdict = backend
        .wait(&handle, Instant::now())
        .expect("wait should succeed");
    assert_eq!(verdict, Verdict::TestFailure);
}

#[test]
fn empty_submit_argv_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = CommandBackend::with_config(CommandConfig {
        submit: vec![],
        logs: vec![
            script_path(&write_logs_mock(dir.path())),
            "{handle}".to_string(),
        ],
        wait: vec![
            script_path(&write_wait_mock(dir.path(), 0)),
            "{handle}".to_string(),
        ],
    });

    let err = backend.submit(&spec()).expect_err("empty argv should fail");
    assert!(err.reason.contains("empty"), "got: {}", err.reason);
}
