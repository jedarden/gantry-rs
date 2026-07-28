// gantry — command-template backend (plan §"Phase 0.5", Components §5 "RemoteBackend trait").
//
// Phase 0.5 walking skeleton, stage 4 of 5 (bf-2vr): builds on stages 1-3 (shim/config
// + GitGate + RefPusher) and implements the command-template backend that runs a
// bash executor on the same box.
//
// The command backend implements RemoteBackend using hardcoded argv arrays:
// - submit: runs the bash executor with the repo/sha/args, captures stdout as handle
// - logs: stub (panics if called)
// - wait: runs the bash executor with the handle, returns exit code -> verdict
//
// Phase 1a will make the argv arrays configurable via config file; the skeleton
// uses hardcoded paths to contrib/gantry-exec.sh.

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict};
use std::io::Write;
use std::process::{Command, Output};
use std::time::Instant;

/// The command-template backend implementation.
///
/// Phase 0.5: hardcoded executor path at `./contrib/gantry-exec.sh`. The backend
/// runs this script with argv-style arguments:
///
/// Submit (stores the run and emits a handle on stdout):
///   contrib/gantry-exec.sh submit <repo_url> <sha> <args_json>
///
/// Wait (polls the run and exits with the run's exit code):
///   contrib/gantry-exec.sh wait <handle>
///
/// Phase 1a will make these argv arrays configurable via config file templates.
pub struct CommandBackend {
    /// Path to the bash executor script.
    executor: String,
}

impl CommandBackend {
    /// Create a new CommandBackend with the hardcoded executor path.
    ///
    /// Phase 0.5: the executor is `./contrib/gantry-exec.sh` (relative to the
    /// current working directory, which is the repo root for the skeleton).
    pub fn new() -> Self {
        CommandBackend {
            executor: "./contrib/gantry-exec.sh".to_string(),
        }
    }

    /// Create a new CommandBackend with a custom executor path (for testing).
    #[cfg(test)]
    pub fn with_executor(executor: &str) -> Self {
        CommandBackend {
            executor: executor.to_string(),
        }
    }

    /// Run the executor with arguments and return its output.
    fn run_executor(&self, args: &[&str]) -> Result<Output, BackendError> {
        Command::new(&self.executor)
            .args(args)
            .output()
            .map_err(|e| BackendError::new(&format!("failed to run executor: {}", e)))
    }

    /// Parse args JSON into a Vec<String> for the executor.
    ///
    /// Phase 0.5: use a simple JSON array format. The args are passed as a JSON
    /// array of strings; the executor parses this and forwards each element to
    /// the command (e.g., cargo test).
    ///
    /// Phase 1a will use serde_json for robust JSON handling.
    fn format_args_json(args: &[String]) -> String {
        // Phase 0.5: simple JSON array encoding
        // This is a minimal implementation that works for simple cases
        let parts: Vec<&str> = args.iter().map(|a| a.as_str()).collect();
        format!("[{}]", parts.join(","))
    }
}

impl Default for CommandBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteBackend for CommandBackend {
    /// Submit a run to the executor.
    ///
    /// Runs: `contrib/gantry-exec.sh submit <repo_url> <sha> <args_json>`
    ///
    /// The executor's stdout is the handle (an opaque string that wait() can use).
    /// The skeleton expects the handle to be a single line of text (whitespace trimmed).
    ///
    /// ## Errors
    ///
    /// Returns Err if:
    /// - The executor binary cannot be run
    /// - The executor exits non-zero (submit failure)
    /// - The executor emits no output on stdout
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError> {
        let args_json = Self::format_args_json(&spec.args);

        let output = self.run_executor(&["submit", &spec.repo_url, &spec.sha, &args_json])?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "executor submit failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let handle = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if handle.is_empty() {
            return Err(BackendError::new("executor emitted empty handle"));
        }

        Ok(RunHandle::new(&handle))
    }

    /// Stream logs from the running run.
    ///
    /// Phase 0.5: not implemented — panics if called.
    /// Phase 1a will implement log streaming for the command backend.
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError> {
        let _ = (h, out);
        panic!("stream_logs is not implemented in Phase 0.5");
    }

    /// Wait for the run to complete and return its verdict.
    ///
    /// Runs: `contrib/gantry-exec.sh wait <handle>`
    ///
    /// The executor exits with the same exit code as the run it executed. The
    /// backend maps this exit code to a Verdict using the minimal ladder:
    /// - 0 -> Pass
    /// - nonzero -> TestFailure
    ///
    /// ## Parameters
    ///
    /// - `h`: The RunHandle returned by submit().
    /// - `deadline`: The deadline for the run (ignored in Phase 0.5 — the executor
    ///   manages its own timeout). Phase 1a will enforce this at the backend level.
    ///
    /// ## Errors
    ///
    /// Returns Err if:
    /// - The executor binary cannot be run
    /// - The executor crashes (not the run itself, but the executor script)
    /// - The handle is invalid or the run is not found
    fn wait(&self, h: &RunHandle, _deadline: Instant) -> Result<Verdict, BackendError> {
        let output = self.run_executor(&["wait", &h.handle])?;

        // The executor's exit code is the run's exit code
        let exit_code = output.status.code().unwrap_or(-1);

        // Map to verdict using the minimal ladder
        Ok(Verdict::from_exit_code(exit_code))
    }

    /// Describe the run for human consumption.
    ///
    /// Phase 0.5: not implemented — panics if called.
    /// Phase 1a will return the handle or a configured URL template.
    fn describe(&self, h: &RunHandle) -> String {
        let _ = h;
        panic!("describe is not implemented in Phase 0.5");
    }

    /// Cancel the running run.
    ///
    /// Phase 0.5: not implemented — panics if called.
    /// Phase 1a will implement cancellation (e.g., a cancel argv for the executor).
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError> {
        let _ = h;
        panic!("cancel is not implemented in Phase 0.5");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Serialize tests that use filesystem
    static FS_MUTEX: Mutex<()> = Mutex::new(());

    // A simple test executor that:
    // - On submit: writes "handle-<args>" to stdout (args becomes the handle)
    // - On wait: exits 0 if handle contains "pass", exits 1 otherwise
    fn create_test_executor(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).expect("create test executor");
        writeln!(
            file,
            r#"#!/usr/bin/env bash
# Minimal test executor for Phase 0.5 skeleton tests

case "$1" in
    submit)
        # Emit a handle based on the args (which is in $4 as JSON)
        # For testing, we use the first arg value as the handle prefix
        handle="handle-$(echo "$4" | sed 's/[^a-zA-Z0-9]/_/g')"
        echo "$handle"
        exit 0
        ;;
    wait)
        # Exit 0 if handle contains "pass", exit 1 otherwise
        if [[ "$2" == *"pass"* ]]; then
            exit 0
        else
            exit 1
        fi
        ;;
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
"#
        )
        .expect("write test executor");

        // Make it executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&path).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&path, perm).expect("set permissions");
        }

        path
    }

    fn create_temp_dir(_suffix: &str) -> tempfile::TempDir {
        tempfile::tempdir_in("/tmp").expect("create temp dir")
    }

    #[test]
    fn test_command_backend_submit_returns_handle() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("submit");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let spec = RunSpec::new("file:///repo", "abc123", vec!["pass".to_string()]);

        let result = backend.submit(&spec);

        assert!(result.is_ok(), "submit should succeed, got: {:?}", result);
        let handle = result.unwrap();
        assert!(
            handle.handle.contains("pass"),
            "handle should contain 'pass', got: {}",
            handle.handle
        );
    }

    #[test]
    fn test_command_backend_wait_pass_returns_pass_verdict() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("wait-pass");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let handle = RunHandle::new("handle-pass");

        let result = backend.wait(&handle, Instant::now());

        assert!(result.is_ok(), "wait should succeed, got: {:?}", result);
        let verdict = result.unwrap();
        assert_eq!(verdict, Verdict::Pass, "wait should return Pass verdict");
    }

    #[test]
    fn test_command_backend_wait_fail_returns_test_failure_verdict() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("wait-fail");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let handle = RunHandle::new("handle-fail");

        let result = backend.wait(&handle, Instant::now());

        assert!(result.is_ok(), "wait should succeed, got: {:?}", result);
        let verdict = result.unwrap();
        assert_eq!(
            verdict,
            Verdict::TestFailure,
            "wait should return TestFailure verdict"
        );
    }

    #[test]
    fn test_command_backend_submit_then_wait_round_trip() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("round-trip");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());

        // Submit with "pass" args
        let spec = RunSpec::new("file:///repo", "abc123", vec!["pass".to_string()]);
        let handle = backend.submit(&spec).expect("submit should succeed");

        // Wait on the handle
        let verdict = backend
            .wait(&handle, Instant::now())
            .expect("wait should succeed");

        assert_eq!(verdict, Verdict::Pass, "round-trip should return Pass");
    }

    #[test]
    fn test_command_backend_submit_then_wait_round_trip_failure() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("round-trip-fail");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());

        // Submit with "fail" args
        let spec = RunSpec::new("file:///repo", "abc123", vec!["fail".to_string()]);
        let handle = backend.submit(&spec).expect("submit should succeed");

        // Wait on the handle
        let verdict = backend
            .wait(&handle, Instant::now())
            .expect("wait should succeed");

        assert_eq!(
            verdict,
            Verdict::TestFailure,
            "round-trip should return TestFailure"
        );
    }

    #[test]
    fn test_format_args_json_simple() {
        let args = vec![
            "test".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ];
        let json = CommandBackend::format_args_json(&args);

        // Simple JSON array encoding
        assert!(json.starts_with("["));
        assert!(json.ends_with("]"));
        assert!(json.contains("test"));
        assert!(json.contains("--nocapture"));
    }

    #[test]
    fn test_verdict_from_exit_code_zero_is_pass() {
        assert_eq!(Verdict::from_exit_code(0), Verdict::Pass);
    }

    #[test]
    fn test_verdict_from_exit_code_nonzero_is_test_failure() {
        assert_eq!(Verdict::from_exit_code(1), Verdict::TestFailure);
        assert_eq!(Verdict::from_exit_code(2), Verdict::TestFailure);
        assert_eq!(Verdict::from_exit_code(-1), Verdict::TestFailure);
        assert_eq!(Verdict::from_exit_code(255), Verdict::TestFailure);
    }

    #[test]
    fn test_verdict_to_exit_code_pass_is_zero() {
        assert_eq!(Verdict::Pass.to_exit_code(), 0);
    }

    #[test]
    fn test_verdict_to_exit_code_test_failure_is_one() {
        assert_eq!(Verdict::TestFailure.to_exit_code(), 1);
    }

    #[test]
    fn test_run_handle_new_creates_handle() {
        let handle = RunHandle::new("test-handle");
        assert_eq!(handle.handle, "test-handle");
    }

    #[test]
    fn test_run_spec_new_creates_spec() {
        let spec = RunSpec::new("file:///repo", "abc123", vec!["test".to_string()]);
        assert_eq!(spec.repo_url, "file:///repo");
        assert_eq!(spec.sha, "abc123");
        assert_eq!(spec.args, vec!["test".to_string()]);
    }

    #[test]
    fn test_backend_error_new_creates_error() {
        let err = BackendError::new("test error");
        assert_eq!(err.reason, "test error");
    }

    #[test]
    fn test_backend_error_display_shows_reason() {
        let err = BackendError::new("test error");
        let formatted = format!("{}", err);
        assert_eq!(formatted, "test error");
    }

    #[test]
    fn test_verdict_display_shows_name() {
        assert_eq!(format!("{}", Verdict::Pass), "Pass");
        assert_eq!(format!("{}", Verdict::TestFailure), "TestFailure");
    }

    #[test]
    fn test_command_backend_default_creates_instance() {
        let backend = CommandBackend::default();
        assert_eq!(backend.executor, "./contrib/gantry-exec.sh");
    }

    #[test]
    #[should_panic(expected = "stream_logs is not implemented")]
    fn test_command_backend_stream_logs_panics() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let mut out = Vec::new();
        let _ = backend.stream_logs(&handle, &mut out);
    }

    #[test]
    #[should_panic(expected = "describe is not implemented")]
    fn test_command_backend_describe_panics() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let _ = backend.describe(&handle);
    }

    #[test]
    #[should_panic(expected = "cancel is not implemented")]
    fn test_command_backend_cancel_panics() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let _ = backend.cancel(&handle);
    }
}
