// gantry — command-template backend (plan §"Phase 1a", Components §5 "RemoteBackend trait").
//
// Phase 1a: implements the command-template backend with configurable argv arrays
// supporting placeholder substitution at the argv level (not shell interpolation).
//
// The backend implements RemoteBackend using four configurable argv arrays:
// - submit: submits the run and emits a handle on stdout
// - wait:   polls the run; its status is the verdict (a verdict.json on stdout when
//           the executor emits one, otherwise its exit code)
// - logs:   streams logs from the running run (optional; absent = a reported error)
// - cancel: cancels the run (optional; absent = a reported error)
//
// Placeholders are substituted at the argv level (S-4 arg hygiene):
// - {repo}: repository URL
// - {rev}: commit SHA
// - {args_json}: JSON array of command arguments
// - {handle}: opaque handle string (for wait/logs/cancel)
//
// Phase 1a: placeholder substitution and configurable argv templates.
// Phase 1b: will add cancel argv and logs streaming.

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict, VerdictJson};
use std::io::{BufRead, Write};
use std::process::{Command, Output};
use std::time::Instant;

/// Extract a verdict from an executor's stdout, if it emitted a verdict.json.
///
/// Scans the lines from the end (the verdict is the executor's last word, and
/// anything it printed before it is none of our business) for a JSON object
/// carrying `schema_version` — the marker that distinguishes an intended
/// verdict.json from incidental JSON in the executor's chatter.
///
/// Returns:
/// - `Some(verdict)` — a verdict.json this client can interpret.
/// - `Some(InfraFailure)` — a verdict.json this client CANNOT interpret (a newer
///   schema version, a malformed body). Contract drift is an infra failure by
///   definition: the remote answered in a language we do not speak, so we have no
///   verdict, and guessing one would be worse than falling back (plan
///   §"Versioning & compatibility").
/// - `None` — no verdict.json at all; the caller applies the exit-code contract
///   (the documented graceful degradation for plain command templates).
fn parse_verdict_stdout(stdout: &str) -> Option<Verdict> {
    let candidate = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{') && line.contains("schema_version"))?;

    match VerdictJson::parse(candidate) {
        Ok(Some(verdict)) => Some(verdict.to_verdict()),
        // `parse` returns Ok(None) for no verdict it can hand back at all; treat it
        // like an absent file rather than inventing a classification.
        Ok(None) => None,
        Err(why) => Some(Verdict::infra_failure(format!(
            "verdict.json contract drift: {why}"
        ))),
    }
}

/// Configuration for the command-template backend.
#[derive(Debug, Clone)]
pub struct CommandConfig {
    /// Submit argv array with placeholders.
    pub submit: Vec<String>,
    /// Wait argv array with placeholders ({handle} substituted).
    pub wait: Vec<String>,
    /// Optional logs argv array with placeholders ({handle} substituted).
    pub logs: Option<Vec<String>>,
    /// Optional cancel argv array with placeholders ({handle} substituted).
    pub cancel: Option<Vec<String>>,
}

impl Default for CommandConfig {
    fn default() -> Self {
        CommandConfig {
            submit: vec![
                "./contrib/gantry-exec.sh".to_string(),
                "submit".to_string(),
                "{repo}".to_string(),
                "{rev}".to_string(),
                "{args_json}".to_string(),
            ],
            wait: vec![
                "./contrib/gantry-exec.sh".to_string(),
                "wait".to_string(),
                "{handle}".to_string(),
            ],
            logs: None,
            cancel: None,
        }
    }
}

/// The command-template backend implementation.
pub struct CommandBackend {
    /// Backend configuration.
    config: CommandConfig,
}

impl CommandBackend {
    /// Create a new CommandBackend with the given configuration.
    pub fn new(config: CommandConfig) -> Self {
        CommandBackend { config }
    }

    /// Create a new CommandBackend with default configuration.
    pub fn default_config() -> Self {
        CommandBackend {
            config: CommandConfig::default(),
        }
    }

    /// Substitute placeholders in an argv array.
    ///
    /// Replaces {repo}, {rev}, {args_json}, and {handle} with their values.
    /// Substitution happens at the argv level (no shell interpolation per S-4).
    fn substitute_argv(argv: &[String], spec: &RunSpec, handle: Option<&str>) -> Vec<String> {
        let args_json = serde_json::to_string(&spec.args).unwrap_or_else(|_| "[]".to_string());

        argv.iter()
            .map(|arg| {
                let mut result = arg.clone();
                result = result.replace("{repo}", &spec.repo_url);
                result = result.replace("{rev}", &spec.sha);
                result = result.replace("{args_json}", &args_json);
                if let Some(h) = handle {
                    result = result.replace("{handle}", h);
                }
                result
            })
            .collect()
    }

    /// Run a command and return its output.
    fn run_command(&self, argv: &[String]) -> Result<Output, BackendError> {
        if argv.is_empty() {
            return Err(BackendError::new("empty argv array"));
        }

        let (exe, args) = argv.split_first().unwrap();
        Command::new(exe)
            .args(args)
            .output()
            .map_err(|e| BackendError::new(&format!("failed to run command: {}", e)))
    }

    /// Stream logs from a command to a writer.
    fn stream_command_logs(
        &self,
        argv: &[String],
        out: &mut dyn Write,
    ) -> Result<(), BackendError> {
        if argv.is_empty() {
            return Err(BackendError::new("empty argv array"));
        }

        let (exe, args) = argv.split_first().unwrap();
        let mut child = Command::new(exe)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::new(&format!("failed to spawn logs command: {}", e)))?;

        // Stream stdout to the writer
        if let Some(stdout) = child.stdout.take() {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                let line = line
                    .map_err(|e| BackendError::new(&format!("failed to read log line: {}", e)))?;
                writeln!(out, "{}", line)
                    .map_err(|e| BackendError::new(&format!("failed to write log line: {}", e)))?;
            }
        }

        // Wait for the command to finish
        let status = child
            .wait()
            .map_err(|e| BackendError::new(&format!("failed to wait for logs command: {}", e)))?;

        if !status.success() {
            // Log streaming is best-effort — don't fail the run if logs fail
            eprintln!(
                "[gantry] warning: logs command exited with status: {:?}",
                status.code()
            );
        }

        Ok(())
    }
}

impl Default for CommandBackend {
    fn default() -> Self {
        Self::default_config()
    }
}

impl RemoteBackend for CommandBackend {
    /// Submit a run to the executor.
    ///
    /// Runs the configured submit argv with placeholder substitution:
    /// - {repo} -> repository URL
    /// - {rev} -> commit SHA
    /// - {args_json} -> JSON array of command arguments
    ///
    /// The command's stdout is the handle (an opaque string that wait() can use).
    /// The handle is trimmed of whitespace and should be a single line.
    ///
    /// ## Errors
    ///
    /// Returns Err if:
    /// - The submit command cannot be run
    /// - The submit command exits non-zero (submit failure)
    /// - The command emits no output on stdout
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError> {
        let argv = Self::substitute_argv(&self.config.submit, spec, None);

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

        Ok(RunHandle::command(&handle))
    }

    /// Stream logs from the running run.
    ///
    /// Phase 1a: if logs argv is configured, stream logs from the command.
    /// Otherwise, returns an error (not a panic — allows graceful degradation).
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError> {
        let handle_str = match h {
            RunHandle::Command { handle } => handle,
            _ => {
                return Err(BackendError::new(
                    "command backend received non-Command handle",
                ))
            }
        };

        let logs_argv = self
            .config
            .logs
            .as_ref()
            .ok_or_else(|| BackendError::new("logs argv not configured"))?;

        let argv = Self::substitute_argv(logs_argv, &RunSpec::placeholder(), Some(handle_str));

        self.stream_command_logs(&argv, out)
    }

    /// Wait for the run to complete and return its verdict.
    ///
    /// Runs the configured wait argv with placeholder substitution:
    /// - {handle} -> opaque handle string from submit()
    ///
    /// The verdict comes from the executor's *status*, never from parsing log text
    /// (INV-3). Two status shapes are supported, in precedence order:
    ///
    /// 1. **verdict.json on stdout** — an executor that emits the versioned schema
    ///    ([`crate::backend::VerdictJson`]) gets the full ladder: OOM and
    ///    deadline-exceeded classify as InfraFailure, a failure class as
    ///    TestFailure, a named gate as GateFailure.
    /// 2. **Exit code only** — the universal contract, for executors that emit no
    ///    verdict.json (the documented graceful degradation):
    ///    - 0 -> Pass
    ///    - 1 -> TestFailure
    ///    - `>= 2` -> InfraFailure (submit/schedule/infra issues — a run that
    ///      never happened must never be reported as a test failure, DD-4)
    ///
    /// Unparseable stdout is not an error: it is simply not a verdict.json, and the
    /// exit-code contract takes over. A *well-formed* verdict.json carrying an
    /// unsupported schema version is different — the client cannot interpret the
    /// remote's answer, which is contract drift and therefore an InfraFailure,
    /// never a misread verdict.
    ///
    /// ## Parameters
    ///
    /// - `h`: The RunHandle returned by submit().
    /// - `deadline`: The deadline for the run (ignored by command backend — the remote
    ///   executor manages its own timeout).
    ///
    /// ## Errors
    ///
    /// Returns Err if:
    /// - The wait command cannot be run
    /// - The wait command crashes (not the run itself, but the executor script)
    /// - The handle is invalid or the run is not found
    fn wait(&self, h: &RunHandle, _deadline: Instant) -> Result<Verdict, BackendError> {
        let handle_str = match h {
            RunHandle::Command { handle } => handle,
            _ => {
                return Err(BackendError::new(
                    "command backend received non-Command handle",
                ))
            }
        };

        let argv =
            Self::substitute_argv(&self.config.wait, &RunSpec::placeholder(), Some(handle_str));

        let output = self.run_command(&argv)?;

        // A verdict.json on stdout is the richer status surface; when the executor
        // emits one it wins over the exit code, which cannot express OOM, deadline,
        // or gate attribution.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(verdict) = parse_verdict_stdout(&stdout) {
            return Ok(verdict);
        }

        // No verdict.json: the exit-code-only contract (the run's exit code is the
        // wait command's exit code). A signal-killed executor withholds a code
        // entirely, which is an infra fault, not a test result — `-1` lands on the
        // InfraFailure rung of `from_exit_code` for exactly that reason.
        let exit_code = output.status.code().unwrap_or(-1);
        Ok(Verdict::from_exit_code(exit_code))
    }

    /// Describe the run for human consumption.
    ///
    /// Phase 1a: returns the handle string for command backend.
    fn describe(&self, h: &RunHandle) -> String {
        match h {
            RunHandle::Command { handle } => handle.clone(),
            _ => "unknown".to_string(),
        }
    }

    /// Cancel the running run.
    ///
    /// Phase 1a: if cancel argv is configured, runs it. Otherwise returns an error.
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError> {
        let handle_str = match h {
            RunHandle::Command { handle } => handle,
            _ => {
                return Err(BackendError::new(
                    "command backend received non-Command handle",
                ))
            }
        };

        let cancel_argv = self
            .config
            .cancel
            .as_ref()
            .ok_or_else(|| BackendError::new("cancel argv not configured"))?;

        let argv = Self::substitute_argv(cancel_argv, &RunSpec::placeholder(), Some(handle_str));

        let output = self.run_command(&argv)?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "cancel command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A test executor whose `wait` behavior is keyed off the handle string.
    ///
    /// Each keyword exercises one rung of the ladder end to end — the same shape a
    /// real executor takes, so these are contract tests and not mock theater:
    ///
    /// - `pass`    → exit 0, no verdict.json          (exit-code contract: Pass)
    /// - `fail`    → exit 1, no verdict.json          (exit-code contract: TestFailure)
    /// - `infra`   → exit 3, no verdict.json          (exit-code contract: InfraFailure)
    /// - `oom`     → exit 1 + verdict.json {oom}      (verdict.json wins: InfraFailure)
    /// - `gate`    → exit 1 + verdict.json {gate}     (verdict.json wins: GateFailure)
    /// - `drift`   → exit 1 + verdict.json v99        (uninterpretable: InfraFailure)
    /// - `chatter` → exit 1 + non-verdict output      (degrades to the exit code)
    fn create_test_executor(dir: &Path) -> PathBuf {
        let path = dir.join("test-executor");
        let mut file = fs::File::create(&path).expect("create test executor");
        file.write_all(
            br#"#!/usr/bin/env bash
case "$1" in
    submit)
        # $2=repo $3=rev $4=args_json. The handle echoes the args so a test can
        # steer `wait` through the submit call it actually made.
        echo "handle-$(echo "$4" | tr -cd 'a-zA-Z0-9')"
        exit 0
        ;;
    wait)
        case "$2" in
            *oom*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":137,"oom":true}'
                exit 1 ;;
            *gate*)
                echo '{"schema_version":1,"phase":"Failed","exit_code":101,"gate_failed":"clippy"}'
                exit 1 ;;
            *drift*)
                echo '{"schema_version":99,"phase":"Succeeded","exit_code":0}'
                exit 1 ;;
            *chatter*)
                echo 'running 3 tests'
                echo '{"not":"a verdict"}'
                exit 1 ;;
            *infra*)  exit 3 ;;
            *pass*)   exit 0 ;;
            *)        exit 1 ;;
        esac
        ;;
    logs)
        echo "log line one"
        echo "log line two"
        exit 0
        ;;
    cancel)
        exit 0
        ;;
    *)
        echo "Unknown command: $1" >&2
        exit 2
        ;;
esac
"#,
        )
        .expect("write test executor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod executor");
        }

        path
    }

    /// A backend wired to the test executor, with logs and cancel configured.
    ///
    /// The script is invoked as `bash <script>` rather than executed directly:
    /// exec'ing a file another thread still holds open for writing fails with
    /// ETXTBSY, and every one of these tests writes its own fixture while the rest
    /// of the suite runs in parallel. Reading the script side-steps that race
    /// without changing what the executor does.
    fn test_backend(executor: &Path) -> CommandBackend {
        let exe = executor.to_str().expect("temp path is utf-8").to_string();
        CommandBackend::new(CommandConfig {
            submit: vec![
                "bash".to_string(),
                exe.clone(),
                "submit".to_string(),
                "{repo}".to_string(),
                "{rev}".to_string(),
                "{args_json}".to_string(),
            ],
            wait: vec![
                "bash".to_string(),
                exe.clone(),
                "wait".to_string(),
                "{handle}".to_string(),
            ],
            logs: Some(vec![
                "bash".to_string(),
                exe.clone(),
                "logs".to_string(),
                "{handle}".to_string(),
            ]),
            cancel: Some(vec![
                "bash".to_string(),
                exe,
                "cancel".to_string(),
                "{handle}".to_string(),
            ]),
        })
    }

    fn spec_with_args(args: &[&str]) -> RunSpec {
        RunSpec::new(
            "cargo",
            "test",
            args.iter().map(|s| s.to_string()).collect(),
            "file:///repo",
            "abc123",
            PathBuf::from("."),
        )
    }

    // --- submit --------------------------------------------------------------

    #[test]
    fn submit_returns_the_executor_stdout_as_the_handle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let handle = backend
            .submit(&spec_with_args(&["pass"]))
            .expect("submit should succeed");

        match handle {
            RunHandle::Command { handle } => {
                assert!(
                    handle.starts_with("handle-") && handle.contains("pass"),
                    "handle must be the executor's stdout, got: {handle}",
                );
            }
            other => panic!("command backend must return a Command handle, got {other:?}"),
        }
    }

    #[test]
    fn submit_failure_is_an_error_not_a_verdict() {
        // A submit that never ran is not a test result: the caller must see Err so
        // the decision engine classifies it as InfraFailure (DD-4).
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("not-an-executor");
        let backend = CommandBackend::new(CommandConfig {
            submit: vec![missing.to_str().unwrap().to_string()],
            wait: vec!["true".to_string()],
            logs: None,
            cancel: None,
        });

        assert!(backend.submit(&spec_with_args(&[])).is_err());
    }

    #[test]
    fn submit_rejects_an_empty_handle() {
        // An executor that prints nothing has told us nothing to wait on.
        let backend = CommandBackend::new(CommandConfig {
            submit: vec!["true".to_string()],
            wait: vec!["true".to_string()],
            logs: None,
            cancel: None,
        });

        let err = backend
            .submit(&spec_with_args(&[]))
            .expect_err("an empty handle must be refused");
        assert!(err.reason.contains("empty handle"), "got: {err}");
    }

    // --- wait: the exit-code contract ---------------------------------------

    #[test]
    fn wait_maps_the_exit_code_contract_across_the_ladder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));
        let deadline = Instant::now();

        assert_eq!(
            backend
                .wait(&RunHandle::command("handle-pass"), deadline)
                .expect("wait runs"),
            Verdict::Pass,
        );
        assert_eq!(
            backend
                .wait(&RunHandle::command("handle-fail"), deadline)
                .expect("wait runs"),
            Verdict::test_failure(1),
        );
        // >= 2 is the executor saying "the run never happened" — infra, and the
        // only rung that may fall back locally.
        let infra = backend
            .wait(&RunHandle::command("handle-infra"), deadline)
            .expect("wait runs");
        assert!(
            matches!(infra, Verdict::InfraFailure { .. }),
            "exit 3 must be InfraFailure, got {infra}",
        );
        assert!(infra.should_fallback());
    }

    #[test]
    fn submit_then_wait_round_trips_a_pass() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let handle = backend
            .submit(&spec_with_args(&["pass"]))
            .expect("submit should succeed");
        assert_eq!(
            backend.wait(&handle, Instant::now()).expect("wait runs"),
            Verdict::Pass,
        );
    }

    #[test]
    fn submit_then_wait_round_trips_a_test_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let handle = backend
            .submit(&spec_with_args(&["fail"]))
            .expect("submit should succeed");
        let verdict = backend.wait(&handle, Instant::now()).expect("wait runs");
        assert_eq!(verdict, Verdict::test_failure(1));
        assert!(
            !verdict.should_fallback(),
            "INV-7: a test failure must never fall back locally",
        );
    }

    // --- wait: the verdict.json contract ------------------------------------

    #[test]
    fn wait_prefers_verdict_json_over_the_exit_code_for_oom() {
        // The executor exits 1 (which the exit-code contract would read as a test
        // failure) while verdict.json says OOMKilled. The richer status wins: an
        // infrastructure cap firing says nothing about the code.
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let verdict = backend
            .wait(&RunHandle::command("handle-oom"), Instant::now())
            .expect("wait runs");
        assert!(
            matches!(&verdict, Verdict::InfraFailure { reason } if reason.contains("OOM")),
            "verdict.json OOM must beat the exit code, got {verdict}",
        );
    }

    #[test]
    fn wait_reads_gate_attribution_from_verdict_json() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let verdict = backend
            .wait(&RunHandle::command("handle-gate"), Instant::now())
            .expect("wait runs");
        assert_eq!(verdict, Verdict::gate_failure("clippy".to_string(), 101));
        assert!(
            !verdict.should_fallback(),
            "INV-7: a gate failure must never fall back locally",
        );
    }

    #[test]
    fn wait_treats_an_uninterpretable_verdict_json_as_contract_drift() {
        // A verdict.json from a newer schema is an answer we cannot read. That is
        // an infra failure, never a guessed verdict (plan §"Versioning").
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        let verdict = backend
            .wait(&RunHandle::command("handle-drift"), Instant::now())
            .expect("wait runs");
        assert!(
            matches!(&verdict, Verdict::InfraFailure { reason } if reason.contains("drift")),
            "an unsupported schema version must be contract drift, got {verdict}",
        );
    }

    #[test]
    fn wait_degrades_to_the_exit_code_when_stdout_is_not_a_verdict() {
        // Executor chatter (including unrelated JSON) is not a verdict.json; the
        // exit-code contract takes over, exactly as for a plain command template.
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));

        assert_eq!(
            backend
                .wait(&RunHandle::command("handle-chatter"), Instant::now())
                .expect("wait runs"),
            Verdict::test_failure(1),
        );
    }

    #[test]
    fn parse_verdict_stdout_ignores_output_without_a_verdict() {
        assert_eq!(parse_verdict_stdout(""), None);
        assert_eq!(
            parse_verdict_stdout("running 3 tests\ntest result: ok\n"),
            None
        );
        assert_eq!(parse_verdict_stdout("{\"not\":\"a verdict\"}"), None);
    }

    #[test]
    fn parse_verdict_stdout_takes_the_last_verdict_line() {
        // The verdict is the executor's last word; earlier output never overrides it.
        let stdout = "compiling\n{\"schema_version\":1,\"phase\":\"Failed\",\"exit_code\":1}\n\
                      {\"schema_version\":1,\"phase\":\"Succeeded\",\"exit_code\":0}\n";
        assert_eq!(parse_verdict_stdout(stdout), Some(Verdict::Pass));
    }

    // --- handles, argv substitution, and the rest of the trait ---------------

    #[test]
    fn a_non_command_handle_is_refused_by_every_entry_point() {
        // The command backend has no idea what to do with an Argo handle; saying so
        // is better than substituting a wrong one into an argv.
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));
        let argo = RunHandle::argo("gantry-x7k2p");
        let mut sink = Vec::new();

        assert!(backend.wait(&argo, Instant::now()).is_err());
        assert!(backend.stream_logs(&argo, &mut sink).is_err());
        assert!(backend.cancel(&argo).is_err());
    }

    #[test]
    fn substitute_argv_replaces_every_placeholder_at_the_argv_level() {
        // S-4: substitution is per-argv-element, so a value containing spaces or
        // shell metacharacters can never split into extra arguments.
        let spec = spec_with_args(&["--", "weird: [args]"]);
        let argv = vec![
            "exec".to_string(),
            "{repo}".to_string(),
            "{rev}".to_string(),
            "{args_json}".to_string(),
            "{handle}".to_string(),
        ];

        let out = CommandBackend::substitute_argv(&argv, &spec, Some("h-1"));

        assert_eq!(
            out.len(),
            argv.len(),
            "substitution must not split arguments"
        );
        assert_eq!(out[1], "file:///repo");
        assert_eq!(out[2], "abc123");
        assert_eq!(out[3], r#"["--","weird: [args]"]"#);
        assert_eq!(out[4], "h-1");
    }

    #[test]
    fn substitute_argv_leaves_the_handle_placeholder_alone_when_absent() {
        // submit() has no handle yet; the placeholder must survive untouched rather
        // than becoming an empty string that silently means something else.
        let spec = spec_with_args(&[]);
        let out = CommandBackend::substitute_argv(&["{handle}".to_string()], &spec, None);
        assert_eq!(out, vec!["{handle}".to_string()]);
    }

    #[test]
    fn stream_logs_writes_the_executor_output_to_the_sink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));
        let mut sink = Vec::new();

        backend
            .stream_logs(&RunHandle::command("handle-pass"), &mut sink)
            .expect("logs stream");

        let text = String::from_utf8(sink).expect("log output is utf-8");
        assert!(
            text.contains("log line one") && text.contains("log line two"),
            "got: {text}"
        );
    }

    #[test]
    fn logs_and_cancel_report_when_they_are_not_configured() {
        // Absent templates are a configuration fact to report, not a panic.
        let backend = CommandBackend::default_config();
        let handle = RunHandle::command("h-1");
        let mut sink = Vec::new();

        assert!(backend.stream_logs(&handle, &mut sink).is_err());
        assert!(backend.cancel(&handle).is_err());
    }

    #[test]
    fn cancel_runs_the_configured_template() {
        let dir = tempfile::tempdir().expect("temp dir");
        let backend = test_backend(&create_test_executor(dir.path()));
        assert!(backend.cancel(&RunHandle::command("handle-pass")).is_ok());
    }

    #[test]
    fn describe_returns_the_handle_for_humans() {
        let backend = CommandBackend::default_config();
        assert_eq!(backend.describe(&RunHandle::command("run-17")), "run-17");
    }

    #[test]
    fn default_config_targets_the_shipped_executor() {
        let config = CommandConfig::default();
        assert_eq!(config.submit[0], "./contrib/gantry-exec.sh");
        assert_eq!(config.wait[0], "./contrib/gantry-exec.sh");
    }

    #[test]
    fn an_empty_argv_template_is_an_error_not_a_panic() {
        let backend = CommandBackend::new(CommandConfig {
            submit: Vec::new(),
            wait: Vec::new(),
            logs: None,
            cancel: None,
        });

        assert!(backend.submit(&spec_with_args(&[])).is_err());
        assert!(backend
            .wait(&RunHandle::command("h-1"), Instant::now())
            .is_err());
    }
}
