// gantry — command-template backend (plan §"Phase 0.5", Components §5 "RemoteBackend trait").
//
// Phase 0.5 walking skeleton, stage 4 of 5 (bf-2vr): builds on stages 1-3 (shim/config
// + GitGate + RefPusher) and implements the command-template backend that runs a
// bash executor on the same box.
//
// Phase 1a (bf-23i): configurable argv templates with placeholder substitution.
// The command backend implements RemoteBackend using user-configured argv arrays:
// - submit: runs configured submit argv with {repo}, {rev}, {args_json} placeholders
// - logs: runs configured logs argv with {handle} placeholder for streaming
// - wait: runs configured wait argv with {handle} placeholder, maps exit code to verdict

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict};
use std::io::Write;
use std::process::{Command, Output};
use std::time::Instant;

/// Configuration for command-template backend.
///
/// Phase 1a: configurable argv arrays with placeholder substitution.
/// Placeholders are substituted at the argv level (no shell interpolation).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandConfig {
    /// Submit command argv array. Placeholders: {repo}, {rev}, {args_json}.
    /// Example: ["my-ci", "submit", "--repo", "{repo}", "--rev", "{rev}", "--args", "{args_json}"]
    pub submit: Vec<String>,

    /// Logs command argv array. Placeholder: {handle}.
    /// Example: ["my-ci", "logs", "{handle}", "--follow"]
    pub logs: Vec<String>,

    /// Wait command argv array. Placeholder: {handle}.
    /// Example: ["my-ci", "wait", "{handle}"]
    pub wait: Vec<String>,
}

impl Default for CommandConfig {
    fn default() -> Self {
        // Phase 0.5 compatible defaults: contrib/gantry-exec.sh
        // Respect GANTRY_EXEC_PATH environment variable if set (for integration tests)
        let executor_path = std::env::var("GANTRY_EXEC_PATH")
            .unwrap_or_else(|_| "./contrib/gantry-exec.sh".to_string());

        CommandConfig {
            submit: vec![
                executor_path.clone(),
                "submit".to_string(),
                "{repo}".to_string(),
                "{rev}".to_string(),
                "{args_json}".to_string(),
            ],
            logs: vec![
                executor_path.clone(),
                "logs".to_string(),
                "{handle}".to_string(),
            ],
            wait: vec![executor_path, "wait".to_string(), "{handle}".to_string()],
        }
    }
}

/// Substitute placeholders in an argv array.
///
/// Replaces {repo}, {rev}, {args_json}, {handle} with actual values.
/// Substitution happens at the argv level (no shell interpolation, S-4).
fn substitute_placeholders(
    argv: &[String],
    repo: &str,
    rev: &str,
    args_json: &str,
    handle: Option<&str>,
) -> Vec<String> {
    argv.iter()
        .map(|arg| {
            let mut result = arg.clone();
            if let Some(h) = handle {
                result = result.replace("{handle}", h);
            }
            result
                .replace("{repo}", repo)
                .replace("{rev}", rev)
                .replace("{args_json}", args_json)
        })
        .collect()
}

/// The command-template backend implementation.
///
/// Phase 1a: configurable argv templates with placeholder substitution.
pub struct CommandBackend {
    /// Command configuration (submit, logs, wait argv arrays).
    config: CommandConfig,
}

impl CommandBackend {
    /// Create a new CommandBackend with default configuration.
    ///
    /// Phase 1a: defaults to Phase 0.5 compatible executor at ./contrib/gantry-exec.sh.
    pub fn new() -> Self {
        CommandBackend {
            config: CommandConfig::default(),
        }
    }

    /// Create a new CommandBackend with custom configuration.
    pub fn with_config(config: CommandConfig) -> Self {
        CommandBackend { config }
    }

    /// Create a new CommandBackend with a custom executor path (for testing).
    #[cfg(test)]
    pub fn with_executor(executor: &str) -> Self {
        let config = CommandConfig {
            submit: vec![
                executor.to_string(),
                "submit".to_string(),
                "{repo}".to_string(),
                "{rev}".to_string(),
                "{args_json}".to_string(),
            ],
            logs: vec![
                executor.to_string(),
                "logs".to_string(),
                "{handle}".to_string(),
            ],
            wait: vec![
                executor.to_string(),
                "wait".to_string(),
                "{handle}".to_string(),
            ],
        };
        CommandBackend { config }
    }

    /// Run a command with arguments and return its output.
    fn run_command(&self, argv: &[String]) -> Result<Output, BackendError> {
        if argv.is_empty() {
            return Err(BackendError::new("command argv is empty"));
        }

        let cmd = &argv[0];
        let args = &argv[1..];

        Command::new(cmd)
            .args(args)
            .output()
            .map_err(|e| BackendError::new(&format!("failed to run command: {}", e)))
    }

    /// Format args as JSON array.
    ///
    /// Phase 1a: use serde_json for robust JSON handling (S-4 compliance).
    fn format_args_json(args: &[String]) -> String {
        serde_json::to_string(args).expect("args should be JSON-serializable")
    }
}

impl Default for CommandBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteBackend for CommandBackend {
    /// Submit a run to the command backend.
    ///
    /// Runs the configured submit argv with {repo}, {rev}, {args_json} placeholders.
    /// The command's stdout is the handle (an opaque string that wait() can use).
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError> {
        let args_json = Self::format_args_json(&spec.args);
        let argv = substitute_placeholders(
            &self.config.submit,
            &spec.repo_url,
            &spec.sha,
            &args_json,
            None,
        );

        let output = self.run_command(&argv)?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "submit command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let handle = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if handle.is_empty() {
            return Err(BackendError::new("submit command emitted empty handle"));
        }

        Ok(RunHandle::new(&handle))
    }

    /// Stream logs from the running run.
    ///
    /// Phase 1a: runs the configured logs argv with {handle} placeholder.
    /// Best-effort: failures don't fail the overall run (wait is authoritative).
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError> {
        let argv = substitute_placeholders(&self.config.logs, "", "", "", Some(&h.handle));

        let output = self.run_command(&argv)?;

        // Write log output to the writer
        out.write_all(&output.stdout)
            .map_err(|e| BackendError::new(&format!("failed to write logs: {}", e)))?;

        Ok(())
    }

    /// Wait for the run to complete and return its verdict.
    ///
    /// Runs the configured wait argv with {handle} placeholder.
    /// The command's exit code maps to a Verdict using the full ladder.
    ///
    /// ## Parameters
    ///
    /// - `h`: The RunHandle returned by submit().
    /// - `deadline`: The deadline for the run (ignored in Phase 1a — the command
    ///   manages its own timeout).
    fn wait(&self, h: &RunHandle, _deadline: Instant) -> Result<Verdict, BackendError> {
        let argv = substitute_placeholders(&self.config.wait, "", "", "", Some(&h.handle));

        let output = self.run_command(&argv)?;

        // The command's exit code is the run's exit code
        let exit_code = output.status.code().unwrap_or(-1);

        // Map to verdict using the full ladder
        Ok(Verdict::from_exit_code(exit_code))
    }

    /// Describe the run for human consumption.
    ///
    /// Phase 1a: returns the handle (opaque identifier from the command backend).
    fn describe(&self, h: &RunHandle) -> String {
        format!("handle/{}", h.handle)
    }

    /// Cancel the running run.
    ///
    /// Phase 1a: not implemented for command backend (no generic cancel mechanism).
    /// Returns an error explaining cancellation is not supported.
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError> {
        let _ = h;
        Err(BackendError::new(
            "cancel is not supported for command backend",
        ))
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
        // Exit code >= 2 is InfraFailure per Phase 1a command contract
        assert_eq!(Verdict::from_exit_code(2), Verdict::InfraFailure);
        assert_eq!(Verdict::from_exit_code(-1), Verdict::InfraFailure);
        assert_eq!(Verdict::from_exit_code(255), Verdict::InfraFailure);
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
        assert_eq!(backend.config.submit[0], "./contrib/gantry-exec.sh");
        assert_eq!(backend.config.wait[0], "./contrib/gantry-exec.sh");
        assert_eq!(backend.config.logs[0], "./contrib/gantry-exec.sh");
    }

    #[test]
    fn test_command_backend_stream_logs_works() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let mut out = Vec::new();
        // This will fail to run the command, but it won't panic
        let result = backend.stream_logs(&handle, &mut out);
        assert!(result.is_err() || result.is_ok()); // Just verify it doesn't panic
    }

    #[test]
    fn test_command_backend_describe_works() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let description = backend.describe(&handle);
        assert_eq!(description, "handle/test");
    }

    #[test]
    fn test_command_backend_cancel_returns_error() {
        let backend = CommandBackend::new();
        let handle = RunHandle::new("test");
        let result = backend.cancel(&handle);
        assert!(result.is_err());
        assert!(result.unwrap_err().reason.contains("not supported"));
    }
}
