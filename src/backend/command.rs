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
// - status: runs configured status argv with {handle} placeholder when the template
//   has a status step; without one it returns a documenting Err (never panics)

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, RunStatus, Verdict};
use std::io::Write;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

/// Poll interval while the wait command runs — the granularity of the
/// deadline check. Frequent enough to expire promptly, cheap enough (one
/// `try_wait` syscall per tick) to never matter against a 40-minute budget.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

    /// Status probe command argv. Placeholder: {handle}.
    ///
    /// The command prints one word on stdout — "pending", "running", or
    /// "completed" (case-insensitive) — mapped to the matching RunStatus;
    /// any other word maps to RunStatus::Unknown. A non-zero probe exit is
    /// an Err (probe failure), not a run state.
    ///
    /// `None` (the default) means the command template has no status step:
    /// status() then returns an Err documenting the limitation rather than
    /// probing or panicking.
    /// Example: Some(["my-ci", "status", "{handle}"])
    pub status: Option<Vec<String>>,
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
            // The reference executor (contrib/gantry-exec.sh) has no status
            // subcommand yet, so the default template has no status step.
            status: None,
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

/// Map a status probe's stdout to a RunStatus.
///
/// The status argv contract: one word on stdout — "pending", "running", or
/// "completed" (case-insensitive, surrounding whitespace tolerated) — mapped
/// to the matching variant. Anything else, including empty output, is
/// RunStatus::Unknown: the probe answered, just not with a state this version
/// knows. Same leniency as FailureClass::from_kebab — a word coined by a
/// newer template must not turn a best-effort snapshot into a hard error.
fn status_from_word(stdout: &str) -> RunStatus {
    match stdout.trim().to_lowercase().as_str() {
        "pending" => RunStatus::Pending,
        "running" => RunStatus::Running,
        "completed" => RunStatus::Completed,
        _ => RunStatus::Unknown,
    }
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

    /// The structured deadline-expiry error for a run's wait command.
    fn deadline_error(h: &RunHandle) -> BackendError {
        BackendError::deadline(&format!(
            "run {} deadline exceeded before the wait command returned a verdict",
            h.handle
        ))
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
            status: None,
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
            .map_err(|e| match e.kind() {
                // NotFound means the configured program itself is missing —
                // say so by name so the user can fix their argv, instead of
                // burying it in an io-error string.
                std::io::ErrorKind::NotFound => {
                    BackendError::new(&format!("command not found: {}", cmd))
                }
                // Everything else (permission denied, ...) keeps the
                // descriptive form: program name plus the underlying io error.
                _ => BackendError::new(&format!("failed to run command {}: {}", cmd, e)),
            })
    }

    /// Spawn a command with arguments without waiting for it.
    ///
    /// Same argv and error mapping as [`Self::run_command`]; the child
    /// inherits this process's stdio, so a wait command that streams remote
    /// logs keeps flowing to the caller's transcript instead of being
    /// captured and discarded — and there is no pipe to fill, so a chatty
    /// child can never deadlock itself into an artificial deadline expiry.
    fn spawn_command(&self, argv: &[String]) -> Result<std::process::Child, BackendError> {
        if argv.is_empty() {
            return Err(BackendError::new("command argv is empty"));
        }

        let cmd = &argv[0];
        let args = &argv[1..];

        Command::new(cmd)
            .args(args)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    BackendError::new(&format!("command not found: {}", cmd))
                }
                _ => BackendError::new(&format!("failed to run command {}: {}", cmd, e)),
            })
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
    /// The wait command is bounded by `deadline` (features.md v1.x
    /// "timeout/deadline config per backend"): the backend polls the child's
    /// exit status and, when the deadline passes first, kills the wait
    /// command and returns the structured [`BackendError::deadline`] — expiry
    /// classifies as InfraFailure upstream (plan DD-4), never as an
    /// exit-code-derived verdict. A deadline that is already elapsed when
    /// wait() is entered skips the spawn entirely.
    ///
    /// ## Parameters
    ///
    /// - `h`: The RunHandle returned by submit().
    /// - `deadline`: The wall-clock instant past which the run must produce
    ///   no more polls, sleeps, or verdicts.
    fn wait(&self, h: &RunHandle, deadline: Instant) -> Result<Verdict, BackendError> {
        // Deadline first: an elapsed budget never spawns the wait command.
        if Instant::now() >= deadline {
            return Err(Self::deadline_error(h));
        }

        let argv = substitute_placeholders(&self.config.wait, "", "", "", Some(&h.handle));
        let mut child = self.spawn_command(&argv)?;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // The command's exit code is the run's exit code; map it
                    // using the full ladder.
                    return Ok(Verdict::from_exit_code(status.code().unwrap_or(-1)));
                }
                Ok(None) => {
                    let now = Instant::now();
                    if now >= deadline {
                        // The run outlived its deadline: kill the wait
                        // command (nothing it returns past this point is a
                        // verdict) and surface the expiry. The reap keeps
                        // the killed child from lingering as a zombie.
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Self::deadline_error(h));
                    }
                    // Sleep toward the next poll, but never past the
                    // deadline — the clamp makes the next loop-top check
                    // land on time.
                    thread::sleep(WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
                }
                Err(e) => {
                    return Err(BackendError::new(&format!(
                        "failed to wait on command: {}",
                        e
                    )));
                }
            }
        }
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

    /// Query the current status of a run without waiting for it to complete.
    ///
    /// When the command template config has a status step, runs it with the
    /// {handle} placeholder and maps the emitted word onto RunStatus via
    /// [`status_from_word`]. When there is no status step (the default — the
    /// reference executor has no status subcommand), returns an Err
    /// documenting the limitation. Either way this never panics, and it never
    /// blocks on the run the way wait() does.
    fn status(&self, h: &RunHandle) -> Result<RunStatus, BackendError> {
        let Some(argv_template) = &self.config.status else {
            return Err(BackendError::new(
                "status is not supported: command template config has no status step",
            ));
        };

        let argv = substitute_placeholders(argv_template, "", "", "", Some(&h.handle));
        let output = self.run_command(&argv)?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "status command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(status_from_word(&stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    // Serialize tests that use filesystem
    static FS_MUTEX: Mutex<()> = Mutex::new(());

    // A simple test executor that:
    // - On submit: writes "handle-<repo path tail>" to stdout and exits 0
    //   immediately — the run itself is NOT performed here
    // - On logs: writes deterministic log lines keyed on the handle
    // - On wait: performs the run, whose exit code is driven by the handle:
    //     "handle-exit-<N>" -> exit N   (any code, e.g. 0, 1, 2, 70)
    //     contains "pass"   -> exit 0
    //     anything else     -> exit 1
    fn create_test_executor(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).expect("create test executor");
        writeln!(
            file,
            r#"#!/usr/bin/env bash
# Minimal test executor for backend tests

case "$1" in
    submit)
        # Emit a handle based on the repo URL (in $2) which contains test identifier
        # For testing, we extract the test identifier from the repo path
        handle=$(echo "$2" | sed 's|.*/||')
        echo "handle-$handle"
        exit 0
        ;;
    logs)
        # Deterministic multi-line content keyed on the handle ($2) so
        # stream_logs tests can assert the exact captured stdout.
        echo "log line for $2"
        echo "second log line for $2"
        exit 0
        ;;
    wait)
        # Handle-driven exit code covering the full verdict ladder.
        if [[ "$2" =~ exit-([0-9]+) ]]; then
            exit "${{BASH_REMATCH[1]}}"
        elif [[ "$2" == *"pass"* ]]; then
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

    /// A deadline comfortably past every mock's runtime — for the tests that
    /// assert verdict mapping, where the budget must never bite. Expiry
    /// behaviour gets its own dedicated tests below.
    fn generous_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn test_command_backend_submit_returns_handle() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("submit");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let spec = RunSpec::new("cargo", "test", vec![], "file:///repo/pass", "abc123", "");

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

        let result = backend.wait(&handle, generous_deadline());

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

        let result = backend.wait(&handle, generous_deadline());

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

        // Submit with "pass" in the repo URL
        let spec = RunSpec::new("cargo", "test", vec![], "file:///repo/pass", "abc123", "");
        let handle = backend.submit(&spec).expect("submit should succeed");

        // Wait on the handle
        let verdict = backend
            .wait(&handle, generous_deadline())
            .expect("wait should succeed");

        assert_eq!(verdict, Verdict::Pass, "round-trip should return Pass");
    }

    #[test]
    fn test_command_backend_submit_then_wait_round_trip_failure() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("round-trip-fail");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());

        // Submit with "fail" in the repo URL
        let spec = RunSpec::new("cargo", "test", vec![], "file:///repo/fail", "abc123", "");
        let handle = backend.submit(&spec).expect("submit should succeed");

        // Wait on the handle
        let verdict = backend
            .wait(&handle, generous_deadline())
            .expect("wait should succeed");

        assert_eq!(
            verdict,
            Verdict::TestFailure,
            "round-trip should return TestFailure"
        );
    }

    #[test]
    fn test_command_backend_submit_returns_without_blocking_on_the_run() {
        // submit() must return once the submit command finishes — the run
        // itself is wait()'s business. The executor here proves it both ways:
        // its wait subcommand is what performs the run, takes RUN_SECONDS,
        // and leaves a marker behind. If submit() had blocked on (or itself
        // executed) the run, the marker would exist when submit returned and
        // submit would have cost RUN_SECONDS — both asserted against below.
        const RUN_SECONDS: u64 = 2;
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("submit-nonblocking");

        let marker = temp_dir.path().join("run-started-marker");
        let executor = write_script(
            temp_dir.path(),
            "test-executor",
            &format!(
                r#"case "$1" in
    submit)
        echo "handle-run-1"
        exit 0
        ;;
    wait)
        # The run itself: marks that it ran, then takes RUN_SECONDS.
        touch "{}"
        sleep {RUN_SECONDS}
        exit 0
        ;;
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
"#,
                marker.display()
            ),
        );

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let spec = RunSpec::new("cargo", "test", vec![], "file:///repo/run", "abc123", "");

        let start = Instant::now();
        let handle = backend.submit(&spec).expect("submit should succeed");
        let submit_elapsed = start.elapsed();

        assert!(
            !marker.exists(),
            "submit returned but the run already executed — submit must not run the run"
        );
        assert!(
            submit_elapsed < Duration::from_secs(RUN_SECONDS),
            "submit must not block on the {RUN_SECONDS}s run, took {submit_elapsed:?}"
        );

        // wait() is what actually performs the run and decides the verdict.
        let start = Instant::now();
        let verdict = backend
            .wait(&handle, generous_deadline())
            .expect("wait should succeed");
        let wait_elapsed = start.elapsed();

        assert_eq!(
            verdict,
            Verdict::Pass,
            "the run's exit code (decided by wait) is the verdict"
        );
        assert!(marker.exists(), "wait should have executed the run");
        assert!(
            wait_elapsed >= Duration::from_secs(RUN_SECONDS),
            "wait should take the run's full {RUN_SECONDS}s, took {wait_elapsed:?}"
        );
    }

    #[test]
    fn test_command_backend_submit_then_wait_round_trip_infra_failure() {
        // The bottom rung of the ladder through the real CommandBackend path:
        // a run exiting 2 — and any higher code — is InfraFailure, not
        // TestFailure. The handle carries the exit code ("handle-exit-<N>")
        // and the executor's wait subcommand exits with exactly that code.
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("round-trip-infra");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());

        for repo_tail in ["exit-2", "exit-70"] {
            let spec = RunSpec::new(
                "cargo",
                "test",
                vec![],
                &format!("file:///repo/{repo_tail}"),
                "abc123",
                "",
            );
            let handle = backend.submit(&spec).expect("submit should succeed");
            assert_eq!(
                handle.handle,
                format!("handle-{repo_tail}"),
                "handle should carry the exit-code driver from the spec"
            );

            let verdict = backend
                .wait(&handle, generous_deadline())
                .expect("wait should succeed");
            assert_eq!(
                verdict,
                Verdict::InfraFailure,
                "run exit code driven by '{repo_tail}' must map to InfraFailure"
            );
        }
    }

    // --- placeholder substitution (bf-3rer) ---

    #[test]
    fn substitute_replaces_all_four_placeholders() {
        let argv = vec![
            "ci".to_string(),
            "submit".to_string(),
            "{repo}".to_string(),
            "{rev}".to_string(),
            "{args_json}".to_string(),
            "{handle}".to_string(),
        ];
        let out = substitute_placeholders(&argv, "file:///r", "abc123", "[\"test\"]", Some("h-1"));
        assert_eq!(
            out,
            vec!["ci", "submit", "file:///r", "abc123", "[\"test\"]", "h-1"]
        );
    }

    #[test]
    fn substitute_embedded_placeholder_inside_larger_arg() {
        let argv = vec!["--repo={repo}".to_string(), "rev:{rev}:head".to_string()];
        let out = substitute_placeholders(&argv, "file:///r", "abc123", "", None);
        assert_eq!(out, vec!["--repo=file:///r", "rev:abc123:head"]);
    }

    #[test]
    fn substitute_repeated_placeholder_in_one_arg() {
        let argv = vec!["{rev}..{rev}".to_string()];
        let out = substitute_placeholders(&argv, "", "abc123", "", None);
        assert_eq!(out, vec!["abc123..abc123"]);
    }

    #[test]
    fn substitute_multiple_distinct_placeholders_in_one_arg() {
        let argv = vec!["{repo}@{rev}#{handle}".to_string()];
        let out = substitute_placeholders(&argv, "file:///r", "abc123", "", Some("h-7"));
        assert_eq!(out, vec!["file:///r@abc123#h-7"]);
    }

    #[test]
    fn substitute_without_placeholders_leaves_argv_unchanged() {
        let argv = vec!["ci".to_string(), "--flag".to_string(), "value".to_string()];
        let out = substitute_placeholders(&argv, "r", "v", "a", Some("h"));
        assert_eq!(out, argv);
    }

    #[test]
    fn substitute_leaves_handle_literal_when_handle_absent() {
        // submit has no handle yet: {handle} must stay literal rather than
        // being silently dropped or replaced with an empty string.
        let argv = vec![
            "ci".to_string(),
            "{handle}".to_string(),
            "x{handle}y".to_string(),
        ];
        let out = substitute_placeholders(&argv, "r", "v", "a", None);
        assert_eq!(out, vec!["ci", "{handle}", "x{handle}y"]);
    }

    #[test]
    fn substitute_unknown_placeholder_left_alone() {
        let argv = vec!["{unknown}".to_string(), "{repo}".to_string()];
        let out = substitute_placeholders(&argv, "r", "v", "a", Some("h"));
        assert_eq!(out, vec!["{unknown}", "r"]);
    }

    #[test]
    fn substitute_empty_argv_yields_empty() {
        assert!(substitute_placeholders(&[], "r", "v", "a", Some("h")).is_empty());
    }

    #[test]
    fn substitute_args_json_with_spaces_stays_single_element() {
        // argv-level substitution (S-4): the JSON blob is ONE argument even
        // though it contains spaces — no shell re-splitting happens.
        let argv = vec!["{args_json}".to_string()];
        let out = substitute_placeholders(&argv, "r", "v", "[\"test\", \"--nocapture\"]", None);
        assert_eq!(out, vec!["[\"test\", \"--nocapture\"]"]);
    }

    #[test]
    fn substitute_each_placeholder_binds_to_its_own_value() {
        // Per-placeholder isolation for {repo} {rev} {args_json} {handle}:
        // each placeholder in its own argv slot must bind to its own value and
        // only its own — no cross-wiring between neighbouring config fields.
        // All four values are distinct so a swap or bleed cannot pass.
        let cases: &[(&str, &str)] = &[
            ("{repo}", "REPO-VAL"),
            ("{rev}", "REV-VAL"),
            ("{args_json}", "ARGS-VAL"),
            ("{handle}", "HANDLE-VAL"),
        ];
        for (placeholder, expected) in cases {
            let argv = vec![placeholder.to_string()];
            let out = substitute_placeholders(
                &argv,
                "REPO-VAL",
                "REV-VAL",
                "ARGS-VAL",
                Some("HANDLE-VAL"),
            );
            assert_eq!(
                out,
                vec![expected.to_string()],
                "placeholder {} must bind to its own value",
                placeholder
            );
        }
    }

    #[test]
    fn substitute_shell_metacharacters_stay_one_argv_element() {
        // S-4, proven at the substitution level: substituted values are argv
        // DATA, never shell INPUT. Every character a shell would act on —
        // quotes, $, backticks, spaces, ; — must stay inside its original
        // single argv slot byte-exact. No splicing, no quoting, no
        // interpolation, no element growth.
        let repo = "file:///ho;st 'sq' \"dq\"";
        let rev = "abc123;`git rev-parse` '$SHA'";
        let args_json = "[\"test -- --nocapture\", \"; rm -rf /tmp/x\", \"`whoami` $USER\"]";
        let handle = "h;'\"$(`id`) y";

        let argv = vec![
            "run".to_string(),
            "{repo}".to_string(),
            "{rev}".to_string(),
            "{args_json}".to_string(),
            "{handle}".to_string(),
            "x{repo};y".to_string(),
        ];
        let out = substitute_placeholders(&argv, repo, rev, args_json, Some(handle));

        // Element count is unchanged: substitution split nothing into extra argv.
        assert_eq!(
            out.len(),
            argv.len(),
            "substitution must not splice a value into more argv elements"
        );
        assert_eq!(out[1], repo);
        assert_eq!(out[2], rev);
        assert_eq!(out[3], args_json);
        assert_eq!(out[4], handle);
        assert_eq!(out[5], format!("x{repo};y"), "embedded placeholder too");
    }

    // --- S-4 end-to-end: substituted argv through a real process spawn -------

    /// A script that prints the number of arguments it received, then every
    /// argument verbatim, one per line — the argv ground truth behind the
    /// end-to-end "single element" assertions below. Any splicing, quoting, or
    /// shell interpolation upstream would grow the count and fragment lines.
    fn write_arg_echo_script(dir: &std::path::Path, name: &str) -> PathBuf {
        write_script(
            dir,
            name,
            "echo \"$#\"\nfor arg in \"$@\"; do echo \"$arg\"; done\n",
        )
    }

    #[test]
    fn submit_spawn_delivers_metacharacter_values_as_one_element_each() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("meta-spawn");

        // End-to-end S-4: the substituted argv goes through a real process
        // spawn with no shell in between, so metacharacter-laden values must
        // arrive byte-exact and unsplit — proven by the executor's own
        // argument count, not assumed from the implementation.
        let echo_argv = write_arg_echo_script(temp_dir.path(), "echo-argv");

        let repo = "file:///ho;st 'sq' \"dq\"";
        let rev = "abc123;`rev-parse` '$SHA'";
        let args = vec![
            "test".to_string(),
            "--".to_string(),
            "weird arg;'`$quoted".to_string(),
        ];
        let args_json = serde_json::to_string(&args).expect("args are JSON-serializable");

        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![
                echo_argv.to_string_lossy().to_string(),
                "{repo}".to_string(),
                "{rev}".to_string(),
                "{args_json}".to_string(),
            ],
            logs: vec![],
            wait: vec![],
            status: None,
        });

        let handle = backend
            .submit(&RunSpec::new("cargo", "test", args, repo, rev, ""))
            .expect("submit should run the echo executor");

        // The handle is the executor's stdout: the count line, then one line
        // per argv element (trimmed only at the outer edges by submit()).
        let lines: Vec<&str> = handle.handle.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "executor must see exactly 3 argv elements, saw: {:?}",
            lines
        );
        assert_eq!(lines[0], "3", "metacharacter values must not split");
        assert_eq!(lines[1], repo);
        assert_eq!(lines[2], rev);
        assert_eq!(lines[3], args_json);
    }

    #[test]
    fn stream_logs_delivers_handle_as_one_element() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("logs-handle");

        let echo_argv = write_arg_echo_script(temp_dir.path(), "echo-argv");
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![],
            logs: vec![
                echo_argv.to_string_lossy().to_string(),
                "{handle}".to_string(),
            ],
            wait: vec![],
            status: None,
        });

        let handle = RunHandle::new("run;'`$9 y");
        let mut out = Vec::new();
        backend
            .stream_logs(&handle, &mut out)
            .expect("stream_logs should run the echo executor");

        let lines: Vec<String> = String::from_utf8_lossy(&out)
            .lines()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(lines.len(), 2, "handle must arrive as exactly one element");
        assert_eq!(lines[0], "1");
        assert_eq!(lines[1], handle.handle);
    }

    #[test]
    fn wait_delivers_handle_as_exactly_one_element() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("wait-handle");

        // The executor exits 0 (Verdict::Pass) only when $1 — the substituted
        // handle — equals the expected literal carried in the template itself
        // as $2 ($0 is the script itself). A spliced, quoted, or interpolated
        // handle fails the compare and lands on Verdict::TestFailure instead.
        let compare = write_script(
            temp_dir.path(),
            "compare-handle",
            "if [ \"$1\" = \"$2\" ]; then exit 0; else exit 1; fi\n",
        );
        let expected = "run;'`$9 y";
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![],
            logs: vec![],
            wait: vec![
                compare.to_string_lossy().to_string(),
                "{handle}".to_string(),
                expected.to_string(),
            ],
            status: None,
        });

        let verdict = backend
            .wait(&RunHandle::new(expected), generous_deadline())
            .expect("wait should run the compare executor");
        assert_eq!(
            verdict,
            Verdict::Pass,
            "handle must arrive intact as one argv element"
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
        let spec = RunSpec::new("cargo", "test", vec![], "file:///repo", "abc123", "");
        assert_eq!(spec.repo_url, "file:///repo");
        assert_eq!(spec.sha, "abc123");
        assert_eq!(spec.subcommand, "test");
        assert_eq!(spec.args, vec![] as Vec<String>);
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
    fn test_command_backend_stream_logs_writes_executor_stdout_to_writer() {
        // Real executor path: the logs subcommand of the executor writes two
        // known lines on stdout; stream_logs must copy them byte-exact into
        // the writer, with the {handle} placeholder substituted.
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("stream-logs");
        let executor = create_test_executor(temp_dir.path(), "test-executor");

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let handle = RunHandle::new("handle-abc123");
        let mut out = Vec::new();

        let result = backend.stream_logs(&handle, &mut out);

        assert!(
            result.is_ok(),
            "stream_logs should succeed, got: {:?}",
            result
        );
        assert_eq!(
            String::from_utf8_lossy(&out),
            "log line for handle-abc123\nsecond log line for handle-abc123\n",
            "captured stdout must equal what the executor's logs subcommand wrote"
        );
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

    // --- status() (probe a run without waiting) ------------------------------

    /// Write an executable bash script into `dir` and return its path.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).expect("create script");
        write!(file, "#!/usr/bin/env bash\n{body}").expect("write script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&path).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&path, perm).expect("set permissions");
        }

        path
    }

    /// A backend whose status step is `echo "$1"`: the substituted {handle}
    /// comes straight back as the emitted status word.
    fn status_echo_backend(dir: &std::path::Path) -> CommandBackend {
        let echo = write_script(dir, "status-echo", "echo \"$1\"\n");
        CommandBackend::with_config(CommandConfig {
            submit: vec![],
            logs: vec![],
            wait: vec![],
            status: Some(vec![
                echo.to_string_lossy().to_string(),
                "{handle}".to_string(),
            ]),
        })
    }

    #[test]
    fn status_from_word_maps_contract_words() {
        assert_eq!(status_from_word("pending"), RunStatus::Pending);
        assert_eq!(status_from_word("running"), RunStatus::Running);
        assert_eq!(status_from_word("completed"), RunStatus::Completed);
        // Case-insensitive and whitespace-tolerant.
        assert_eq!(status_from_word("Running\n"), RunStatus::Running);
        assert_eq!(status_from_word("  COMPLETED  "), RunStatus::Completed);
        // Anything else — including empty output — is Unknown, not an error.
        assert_eq!(status_from_word("queued"), RunStatus::Unknown);
        assert_eq!(status_from_word(""), RunStatus::Unknown);
    }

    #[test]
    fn status_configured_argv_probes_run_and_maps_word() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("status-probe");

        let backend = status_echo_backend(temp_dir.path());

        // The handle is substituted for {handle} and the emitted word maps.
        let status = backend
            .status(&RunHandle::new("running"))
            .expect("status probe should succeed");
        assert_eq!(status, RunStatus::Running);

        let status = backend
            .status(&RunHandle::new("completed"))
            .expect("status probe should succeed");
        assert_eq!(status, RunStatus::Completed);
    }

    #[test]
    fn status_without_configured_step_returns_documenting_error() {
        // Default config has no status step: Err (documenting the
        // limitation), never a panic and never a guessed state.
        let backend = CommandBackend::new();
        let result = backend.status(&RunHandle::new("run-1"));

        assert!(result.is_err(), "unconfigured status must Err");
        let err = result.unwrap_err();
        assert!(
            err.reason.contains("no status step"),
            "error should document the missing status step, got: {}",
            err.reason
        );
    }

    #[test]
    fn status_probe_nonzero_exit_is_error_not_run_state() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("status-fail");

        let failing = write_script(
            temp_dir.path(),
            "status-fail",
            "echo 'probe blew up' >&2\nexit 3\n",
        );
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![],
            logs: vec![],
            wait: vec![],
            status: Some(vec![
                failing.to_string_lossy().to_string(),
                "{handle}".to_string(),
            ]),
        });

        let result = backend.status(&RunHandle::new("run-1"));
        assert!(result.is_err(), "failing probe must Err");
        assert!(
            result.unwrap_err().reason.contains("status command failed"),
            "error should name the probe failure"
        );
    }

    // --- spawn-failure error handling (run_command) ---------------------------

    #[test]
    fn run_command_missing_program_names_it_in_error() {
        // NotFound from the spawn itself must name the missing program so the
        // user can fix their configured argv, not just echo an io error.
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec!["gantry-no-such-binary-4f2a".to_string()],
            logs: vec![],
            wait: vec![],
            status: None,
        });

        let err = backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                vec![],
                "file:///repo/pass",
                "abc123",
                "",
            ))
            .expect_err("missing program must Err");
        assert!(
            err.reason.contains("command not found"),
            "error should say command not found, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("gantry-no-such-binary-4f2a"),
            "error should name the missing program, got: {}",
            err.reason
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_command_non_not_found_failure_names_program_and_io_error() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("spawn-denied");

        // A file that exists but carries no execute bit: the spawn fails with
        // PermissionDenied (even for root — execve needs at least one x bit),
        // which must stay a descriptive error naming both the program and the
        // underlying io error.
        let path = write_script(temp_dir.path(), "not-executable", "exit 0\n");
        let mut perm = fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o644);
        fs::set_permissions(&path, perm).expect("clear exec bit");

        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![path.to_string_lossy().to_string()],
            logs: vec![],
            wait: vec![],
            status: None,
        });

        let err = backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                vec![],
                "file:///repo/pass",
                "abc123",
                "",
            ))
            .expect_err("non-executable program must Err");
        assert!(
            err.reason.contains("failed to run command"),
            "error should be the descriptive spawn failure, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("not-executable"),
            "error should name the program, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("denied") || err.reason.contains("Denied"),
            "error should include the underlying io error, got: {}",
            err.reason
        );
    }

    #[test]
    fn submit_nonzero_exit_returns_err_with_stderr() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("submit-fail");

        let failing = write_script(
            temp_dir.path(),
            "submit-fail",
            "echo 'queue rejected the run' >&2\nexit 4\n",
        );
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![failing.to_string_lossy().to_string()],
            logs: vec![],
            wait: vec![],
            status: None,
        });

        let err = backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                vec![],
                "file:///repo/pass",
                "abc123",
                "",
            ))
            .expect_err("non-zero submit exit must Err");
        assert!(
            err.reason.contains("submit command failed"),
            "error should name the submit failure, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("queue rejected the run"),
            "error should carry the stderr text, got: {}",
            err.reason
        );
    }

    #[test]
    fn submit_zero_exit_with_empty_stdout_returns_empty_handle_error() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("submit-empty");

        let quiet = write_script(temp_dir.path(), "submit-empty", "exit 0\n");
        let backend = CommandBackend::with_config(CommandConfig {
            submit: vec![quiet.to_string_lossy().to_string()],
            logs: vec![],
            wait: vec![],
            status: None,
        });

        let err = backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                vec![],
                "file:///repo/pass",
                "abc123",
                "",
            ))
            .expect_err("empty handle must Err");
        assert!(
            err.reason.contains("empty handle"),
            "error should be the empty-handle error, got: {}",
            err.reason
        );
    }

    // --- empty configured argv yields BackendError, never a panic -----------
    //
    // Every config field is user-supplied, so each entry point must treat an
    // empty template as a configuration error surfaced through BackendError —
    // these tests fail on a panic just as hard as on a wrong return.

    fn empty_config() -> CommandConfig {
        CommandConfig {
            submit: vec![],
            logs: vec![],
            wait: vec![],
            status: None,
        }
    }

    #[test]
    fn empty_submit_argv_yields_backend_error_not_panic() {
        let backend = CommandBackend::with_config(empty_config());

        let err = backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                vec![],
                "file:///repo/pass",
                "abc123",
                "",
            ))
            .expect_err("empty submit argv must Err");
        assert!(
            err.reason.contains("argv is empty"),
            "error should name the empty argv, got: {}",
            err.reason
        );
    }

    #[test]
    fn empty_wait_argv_yields_backend_error_not_panic() {
        let backend = CommandBackend::with_config(empty_config());

        let err = backend
            .wait(&RunHandle::new("run-1"), generous_deadline())
            .expect_err("empty wait argv must Err");
        assert!(
            err.reason.contains("argv is empty"),
            "error should name the empty argv, got: {}",
            err.reason
        );
    }

    // --- deadline enforcement (features.md v1.x per-backend deadline) -------

    #[test]
    fn wait_with_an_elapsed_deadline_never_spawns_the_wait_command() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("deadline-elapsed");

        // The wait subcommand would prove it ran by leaving a marker behind;
        // an already-spent budget must skip the spawn entirely, so the
        // marker stays absent and the structured expiry error comes back.
        let marker = temp_dir.path().join("wait-ran-marker");
        let executor = write_script(
            temp_dir.path(),
            "test-executor",
            &format!(
                r#"case "$1" in
    wait)
        touch "{}"
        exit 0
        ;;
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
"#,
                marker.display()
            ),
        );

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let err = backend
            .wait(
                &RunHandle::new("handle-late"),
                Instant::now() - Duration::from_secs(1),
            )
            .expect_err("an elapsed deadline must Err, never run the wait command");

        assert!(
            err.deadline_exceeded,
            "the error must be the structured deadline expiry, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("handle-late"),
            "the deadline error must name the run, got: {}",
            err.reason
        );
        assert!(
            !marker.exists(),
            "an elapsed budget must never spawn the wait command"
        );
    }

    #[test]
    fn wait_kills_a_run_that_outlives_its_deadline_and_returns_no_verdict() {
        let _lock = FS_MUTEX.lock().unwrap();
        let temp_dir = create_temp_dir("deadline-expires");

        // The wait subcommand sleeps far past the deadline: expiry must kill
        // it promptly (no orphan left behind) and surface the structured
        // error — never an exit-code-derived verdict (DD-4).
        let executor = write_script(
            temp_dir.path(),
            "test-executor",
            r#"case "$1" in
    wait)
        sleep 30
        exit 0
        ;;
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
"#,
        );

        let backend = CommandBackend::with_executor(executor.to_str().unwrap());
        let start = Instant::now();
        let err = backend
            .wait(
                &RunHandle::new("handle-slow"),
                Instant::now() + Duration::from_millis(250),
            )
            .expect_err("a run past its deadline must Err, not return a verdict");
        let elapsed = start.elapsed();

        assert!(
            err.deadline_exceeded,
            "the error must be the structured deadline expiry, got: {}",
            err.reason
        );
        assert!(
            err.reason.contains("handle-slow"),
            "the deadline error must name the run, got: {}",
            err.reason
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "expiry must kill the wait command promptly, took {elapsed:?}"
        );
    }

    #[test]
    fn empty_logs_argv_yields_backend_error_not_panic() {
        let backend = CommandBackend::with_config(empty_config());

        let mut out = Vec::new();
        let err = backend
            .stream_logs(&RunHandle::new("run-1"), &mut out)
            .expect_err("empty logs argv must Err");
        assert!(
            err.reason.contains("argv is empty"),
            "error should name the empty argv, got: {}",
            err.reason
        );
    }

    #[test]
    fn empty_status_argv_yields_backend_error_not_panic() {
        // Some(vec![]) passes the "no status step" guard — a configured-but-
        // empty status argv must still surface as an error, not a panic.
        let backend = CommandBackend::with_config(CommandConfig {
            status: Some(vec![]),
            ..empty_config()
        });

        let err = backend
            .status(&RunHandle::new("run-1"))
            .expect_err("empty status argv must Err");
        assert!(
            err.reason.contains("argv is empty"),
            "error should name the empty argv, got: {}",
            err.reason
        );
    }
}
