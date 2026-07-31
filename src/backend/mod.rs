// gantry — RemoteBackend trait and Verdict enum (plan §"Phase 0.5", Components §5).
//
// Phase 0.5 walking skeleton, stage 4 of 5 (bf-2vr): builds on stages 1-3 (shim/config
// + GitGate + RefPusher) and implements the minimal RemoteBackend trait with a
// command-template backend that runs a bash executor on the same box.
//
// This module defines:
// - Verdict: the exit-code ladder for this phase (0 = Pass, nonzero = TestFailure)
// - BackendError: minimal error type for backend operations
// - RunHandle: opaque handle returned by submit() and consumed by wait()
// - RemoteBackend trait: submit / stream_logs / wait / describe / cancel

use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;

/// Verdict: the terminal classification of a run (plan §"Data models", "Verdict semantics").
///
/// Phase 1a implements the full verdict ladder:
/// - Pass: remote ran suite, passed
/// - TestFailure: remote ran suite, failed (tests, compilation, etc.)
/// - GateFailure: an enabled quality gate failed while tests passed
/// - InfraFailure: no verdict produced (push/submit/schedule/stream/deadline issues)
/// - Cancelled: user cancelled the run (Ctrl-C)
/// - Superseded: a newer sha for the same (repo, args) cancelled this run (v1.x)
///
/// The key invariant (INV-7): TestFailure and GateFailure NEVER trigger local retry.
/// Only InfraFailure triggers capped local fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The remote ran the suite and it passed.
    Pass,
    /// The remote ran the suite and it failed (tests, compilation, etc.).
    TestFailure {
        /// The actual exit code from the test run (preserved for faithful exit code).
        exit_code: i32,
    },
    /// An enabled quality gate failed while tests passed.
    ///
    /// This is distinguished from TestFailure so agents can fix lints rather than tests.
    /// Never triggers local retry (INV-7).
    GateFailure {
        /// Exit code from the gate check.
        exit_code: i32,
        /// Which gate failed (e.g., "clippy", "fmt").
        gate_name: String,
    },
    /// No verdict produced due to infrastructure issues.
    ///
    /// This covers: push/submit failures, scheduling issues, stream interruption,
    /// client deadline exceeded, pod OOMKilled, workflow deadline exceeded, etc.
    /// Triggers capped local fallback.
    InfraFailure {
        /// Human-readable reason for the infrastructure failure.
        reason: String,
    },
    /// User cancelled the run (Ctrl-C).
    Cancelled,
    /// A newer sha for the same (repo, args) cancelled this run (v1.x feature).
    Superseded {
        /// The newer sha that superseded this run.
        newer_sha: String,
    },
}

impl Verdict {
    /// Convert an exit code to a Verdict using the full ladder.
    ///
    /// Phase 1a: command backend exit code semantics:
    /// - 0 -> Pass
    /// - 1 -> TestFailure (tests failed)
    /// - >= 2 -> InfraFailure (submit/schedule/infra issues)
    ///
    /// This matches the documented command backend contract where the remote
    /// executor classifies failures via exit code.
    ///
    /// For Argo, the verdict comes from status.phase and verdict.json, not from
    /// kubectl's exit code, so this is used primarily for the command backend.
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Verdict::Pass,
            1 => Verdict::TestFailure { exit_code: 1 },
            _ => Verdict::InfraFailure {
                reason: format!("backend exit code {}", code),
            },
        }
    }

    /// Convert the verdict to an exit code for the caller.
    ///
    /// Phase 1a:
    /// - Pass -> 0
    /// - TestFailure -> preserved exit code
    /// - GateFailure -> preserved exit code (non-zero)
    /// - InfraFailure -> 1 (will trigger local fallback at caller)
    /// - Cancelled -> 130 (standard SIGINT exit code)
    /// - Superseded -> 0 (run didn't produce a verdict, not a failure)
    pub fn to_exit_code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::TestFailure { exit_code } => exit_code.max(1), // Ensure at least 1
            Verdict::GateFailure { exit_code, .. } => exit_code.max(1),
            Verdict::InfraFailure { .. } => 1, // Will trigger local fallback
            Verdict::Cancelled => 130,         // Standard SIGINT exit code
            Verdict::Superseded { .. } => 0,   // Not a failure, just superseded
        }
    }

    /// Check if this verdict should trigger a local fallback run.
    ///
    /// Only InfraFailure triggers local fallback per INV-7.
    /// TestFailure and GateFailure NEVER retry locally.
    pub fn should_fallback(&self) -> bool {
        matches!(self, Verdict::InfraFailure { .. })
    }

    /// Create a test failure verdict from an exit code.
    pub fn test_failure(exit_code: i32) -> Self {
        Verdict::TestFailure {
            exit_code: exit_code.max(1),
        }
    }

    /// Create a gate failure verdict.
    pub fn gate_failure(gate_name: String, exit_code: i32) -> Self {
        Verdict::GateFailure {
            gate_name,
            exit_code: exit_code.max(1),
        }
    }

    /// Create an infra failure verdict with a reason.
    pub fn infra_failure(reason: String) -> Self {
        Verdict::InfraFailure { reason }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Pass => write!(f, "Pass"),
            Verdict::TestFailure { .. } => write!(f, "TestFailure"),
            Verdict::GateFailure { gate_name, .. } => write!(f, "GateFailure({})", gate_name),
            Verdict::InfraFailure { reason } => write!(f, "InfraFailure({})", reason),
            Verdict::Cancelled => write!(f, "Cancelled"),
            Verdict::Superseded { .. } => write!(f, "Superseded"),
        }
    }
}

/// Error type for backend operations.
///
/// Phase 0.5: minimal string-based error type.
/// Phase 1a will expand this to include InfraFailure classification.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendError {
    /// Human-readable reason for the error.
    pub reason: String,
}

impl BackendError {
    /// Create a new BackendError with a reason.
    pub fn new(reason: &str) -> Self {
        BackendError {
            reason: reason.to_string(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for BackendError {}

/// RunHandle: opaque handle returned by submit() and consumed by wait().
///
/// Phase 1a: structured handle with backend-specific state.
/// - Argo: workflow name and pod name for streaming/polling
/// - Command: opaque handle string (stdout of submit command)
#[derive(Debug, Clone, PartialEq)]
pub enum RunHandle {
    /// Argo Workflow handle with workflow name and optional pod name.
    Argo {
        /// Workflow name (e.g., "gantry-x7k2p").
        workflow_name: String,
        /// Pod name for log streaming (None if not yet created).
        pod_name: Option<String>,
    },
    /// Command backend handle with opaque string identifier.
    Command {
        /// Opaque handle string (output of submit command's stdout).
        handle: String,
    },
}

impl RunHandle {
    /// Create a new Command handle from a string.
    pub fn command(handle: &str) -> Self {
        RunHandle::Command {
            handle: handle.to_string(),
        }
    }

    /// Create a new Argo handle with workflow name.
    pub fn argo(workflow_name: &str) -> Self {
        RunHandle::Argo {
            workflow_name: workflow_name.to_string(),
            pod_name: None,
        }
    }

    /// Set the pod name for an Argo handle (used when pod is created).
    pub fn with_pod_name(self, pod_name: String) -> Self {
        match self {
            RunHandle::Argo {
                workflow_name,
                pod_name: _,
            } => RunHandle::Argo {
                workflow_name,
                pod_name: Some(pod_name),
            },
            _ => self, // Ignore pod name setting for non-Argo handles
        }
    }
}

/// RemoteBackend trait: the interface all remote executors must implement.
///
/// Phase 0.5: only submit() and wait() need real bodies; stream_logs, describe,
/// and cancel may panic. The command backend implements this trait using hardcoded
/// argv arrays that invoke a local bash executor.
///
/// Phase 1a will add streaming, cancellation, and describe support; the Argo
/// backend will implement the full trait.
pub trait RemoteBackend {
    /// Submit a run to the remote executor.
    ///
    /// Returns a RunHandle that wait() can use to poll the result. The handle is
    /// opaque to the caller — only the backend implementation knows its structure.
    ///
    /// Phase 0.5: submit runs the configured submit argv and captures its stdout
    /// as the handle. Failure (argv not found, non-zero exit) returns Err.
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError>;

    /// Stream logs from the remote run to a writer (best-effort).
    ///
    /// Phase 0.5: may panic — this is not implemented in the skeleton.
    /// Phase 1a will implement this for both command and Argo backends.
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn std::io::Write) -> Result<(), BackendError> {
        let _ = (h, out);
        panic!("stream_logs is not implemented in Phase 0.5");
    }

    /// Wait for the remote run to complete and return its verdict.
    ///
    /// This is the authoritative source for the run's outcome — log streaming
    /// is best-effort, but wait() must always return a real verdict or an error.
    ///
    /// Phase 0.5: wait runs the configured wait argv with the handle and maps
    /// the exit code to a Verdict using the minimal ladder.
    fn wait(&self, h: &RunHandle, deadline: std::time::Instant) -> Result<Verdict, BackendError>;

    /// Describe a run for human consumption (e.g., a URL to view logs).
    ///
    /// Phase 0.5: may panic — this is not implemented in the skeleton.
    /// Phase 1a will return workflow URLs or similar for Argo; command backends
    /// may return the handle or a configured URL template.
    fn describe(&self, h: &RunHandle) -> String {
        let _ = h;
        panic!("describe is not implemented in Phase 0.5");
    }

    /// Cancel a running run (Ctrl-C propagation).
    ///
    /// Phase 0.5: may panic — this is not implemented in the skeleton.
    /// Phase 1a will implement cancellation for both backends (kubectl delete
    /// for Argo, a cancel argv for command templates).
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError> {
        let _ = h;
        panic!("cancel is not implemented in Phase 0.5");
    }
}

/// RunSpec: the specification of a run to submit to the remote backend.
///
/// Phase 1a: complete struct with all fields needed for remote execution.
/// The remote executor clones the repo, checks out the SHA, and runs the
/// command in the specified directory (relative to repo root).
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    /// Tool name (e.g., "cargo").
    pub tool: String,
    /// Subcommand (e.g., "test").
    pub subcommand: String,
    /// Arguments to pass to the command (e.g., ["--", "--nocapture"]).
    pub args: Vec<String>,
    /// Repository URL (file:// for local testing, git@ or https:// for production).
    pub repo_url: String,
    /// Commit SHA to run (the content the executor checks out).
    pub sha: String,
    /// Caller's directory relative to repo root (for workspace-member support).
    pub cwd_rel: PathBuf,
}

impl RunSpec {
    /// Create a new RunSpec with all fields.
    pub fn new(
        tool: &str,
        subcommand: &str,
        args: Vec<String>,
        repo_url: &str,
        sha: &str,
        cwd_rel: PathBuf,
    ) -> Self {
        RunSpec {
            tool: tool.to_string(),
            subcommand: subcommand.to_string(),
            args,
            repo_url: repo_url.to_string(),
            sha: sha.to_string(),
            cwd_rel,
        }
    }

    /// Create a placeholder RunSpec for argv substitution (used in logs/wait/cancel).
    ///
    /// This creates a minimal RunSpec with placeholder values for when we only need
    /// to substitute {handle} and don't have the full RunSpec available.
    pub fn placeholder() -> Self {
        RunSpec {
            tool: String::new(),
            subcommand: String::new(),
            args: Vec::new(),
            repo_url: String::new(),
            sha: String::new(),
            cwd_rel: PathBuf::new(),
        }
    }

    /// Get the full command argv for the tool (tool, subcommand, and args).
    ///
    /// This is what gets sent to the remote executor as the argv to execute.
    pub fn full_argv(&self) -> Vec<String> {
        let mut argv = vec![self.tool.clone(), self.subcommand.clone()];
        argv.extend(self.args.clone());
        argv
    }
}

pub mod argo;
pub mod command;

/// Versioned verdict.json output from remote executors.
///
/// This schema is emitted by remote executors (e.g., Argo Workflows) to provide
/// structured verdict information beyond exit codes. Consumers ignore unknown fields
/// (additive evolution). An absent verdict.json degrades gracefully to exit-code-only
/// interpretation (command templates without verdict support).
///
/// Schema version 1 (Phase 1a):
/// - schema_version: 1
/// - phase: Succeeded/Failed/Error
/// - exit_code: the test run's exit code
/// - oom: true if pod was OOMKilled
/// - deadline_exceeded: true if workflow exceeded its deadline
/// - failure_class: optional classification (compile-error, test-failure, doctest, harness-panic)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictJsonFailureClass {
    /// Compilation error (could not build).
    CompileError,
    /// Test failure (tests ran but failed).
    TestFailure,
    /// Doctest failure.
    Doctest,
    /// Test harness panic.
    HarnessPanic,
}

/// Versioned verdict.json schema.
#[derive(Debug, Deserialize)]
pub struct VerdictJson {
    /// Schema version (must be 1 for Phase 1a).
    #[serde(rename = "schema_version")]
    pub schema_version: u32,
    /// Workflow phase (Succeeded/Failed/Error).
    pub phase: String,
    /// The test run's exit code.
    #[serde(rename = "exit_code")]
    pub exit_code: i32,
    /// True if the pod was OOMKilled.
    #[serde(default)]
    pub oom: bool,
    /// True if the workflow exceeded its deadline.
    #[serde(rename = "deadline_exceeded", default)]
    pub deadline_exceeded: bool,
    /// Optional failure classification.
    #[serde(rename = "failure_class", default)]
    pub failure_class: Option<VerdictJsonFailureClass>,
    /// Name of the enabled quality gate that failed, when one did.
    ///
    /// Present only when a gate (clippy, fmt, …) failed; the name is what the
    /// verdict is attributed to, so an agent fixes the lint rather than the tests.
    /// Absent for every executor that runs no gates — an omitted field is not a
    /// gate pass, it is "no gates ran", and both degrade to the same exit-code
    /// interpretation.
    #[serde(rename = "gate_failed", default)]
    pub gate_failed: Option<String>,
    /// Additional fields (for future compatibility).
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

impl VerdictJson {
    /// Parse verdict.json from a JSON string.
    ///
    /// Returns Err if the JSON is invalid or schema_version is unsupported.
    /// Returns Ok(None) if the JSON is valid but represents a future schema version
    /// (consumers ignore unknown fields, but schema_version must be recognized).
    pub fn parse(json: &str) -> Result<Option<Self>, String> {
        let parsed: serde_json::Result<Self> = serde_json::from_str(json);
        match parsed {
            Ok(verdict) => {
                // Check schema version
                match verdict.schema_version {
                    1 => Ok(Some(verdict)),
                    version => Err(format!("unsupported schema version: {}", version)),
                }
            }
            Err(e) => Err(format!("failed to parse verdict.json: {}", e)),
        }
    }

    /// Convert the verdict.json to a Verdict enum.
    ///
    /// This implements the full verdict ladder semantics, in precedence order:
    /// - OOMKilled or deadline exceeded -> InfraFailure (an infrastructure cap
    ///   firing says nothing about the code; reporting it as a test failure would
    ///   fake a result)
    /// - Any failure_class -> TestFailure (the suite itself is what failed)
    /// - gate_failed with no failure_class -> GateFailure attributed to that gate
    ///   (tests passed, an enabled gate did not)
    /// - Succeeded -> Pass; Failed/Error with no classification -> TestFailure
    /// - Any other phase -> InfraFailure (an unreadable status is not a verdict)
    ///
    /// Only the InfraFailure rungs ever reach a local run
    /// ([`crate::decision::decide_retry`], INV-7).
    pub fn to_verdict(self) -> Verdict {
        // Check for infrastructure failures first (OOM or deadline)
        if self.oom || self.deadline_exceeded {
            return Verdict::infra_failure(
                if self.oom {
                    "pod was OOMKilled"
                } else {
                    "workflow deadline exceeded"
                }
                .to_string(),
            );
        }

        // Parse failure_class if present
        if let Some(class) = self.failure_class {
            match class {
                // All failure classes are test failures
                VerdictJsonFailureClass::CompileError
                | VerdictJsonFailureClass::TestFailure
                | VerdictJsonFailureClass::Doctest
                | VerdictJsonFailureClass::HarnessPanic => {
                    return Verdict::test_failure(self.exit_code.max(1));
                }
            }
        }

        // No failure_class: a named gate failure is the suite passing and a gate
        // not. It is red and attributed, and — like TestFailure — never retries
        // locally (plan §"Verdict semantics", INV-7).
        if let Some(gate_name) = self.gate_failed {
            return Verdict::gate_failure(gate_name, self.exit_code.max(1));
        }

        // No classification at all: fall back to phase and exit code
        match self.phase.as_str() {
            "Succeeded" => Verdict::Pass,
            "Failed" | "Error" => Verdict::test_failure(self.exit_code.max(1)),
            _ => Verdict::infra_failure(format!("unknown phase: {}", self.phase)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verdict_json_parse_succeeded() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Succeeded",
            "exit_code": 0
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert_eq!(verdict.schema_version, 1);
        assert_eq!(verdict.phase, "Succeeded");
        assert_eq!(verdict.exit_code, 0);
    }

    #[test]
    fn test_verdict_json_parse_failed_with_class() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "failure_class": "test_failure"
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert_eq!(verdict.phase, "Failed");
        assert!(matches!(
            verdict.failure_class,
            Some(VerdictJsonFailureClass::TestFailure)
        ));
    }

    #[test]
    fn test_verdict_json_parse_oom() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 137,
            "oom": true
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert!(verdict.oom);
        assert_eq!(
            verdict.to_verdict(),
            Verdict::infra_failure("pod was OOMKilled".to_string())
        );
    }

    #[test]
    fn test_verdict_json_parse_deadline_exceeded() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "deadline_exceeded": true
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert!(verdict.deadline_exceeded);
        assert_eq!(
            verdict.to_verdict(),
            Verdict::infra_failure("workflow deadline exceeded".to_string())
        );
    }

    #[test]
    fn test_verdict_json_parse_unknown_field_ignored() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Succeeded",
            "exit_code": 0,
            "future_field": "some_value"
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert_eq!(verdict.to_verdict(), Verdict::Pass);
    }

    #[test]
    fn test_verdict_json_parse_unsupported_version() {
        let json = r#"{
            "schema_version": 2,
            "phase": "Succeeded",
            "exit_code": 0
        }"#;
        assert!(VerdictJson::parse(json).is_err());
    }

    #[test]
    fn test_verdict_json_parse_invalid_json() {
        let json = "not json";
        assert!(VerdictJson::parse(json).is_err());
    }

    #[test]
    fn test_verdict_json_to_verdict_compile_error() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 101,
            "failure_class": "compile_error"
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert!(matches!(verdict.to_verdict(), Verdict::TestFailure { .. }));
    }

    #[test]
    fn test_verdict_json_to_verdict_doctest() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "failure_class": "doctest"
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert!(matches!(verdict.to_verdict(), Verdict::TestFailure { .. }));
    }

    #[test]
    fn test_verdict_json_to_verdict_harness_panic() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "failure_class": "harness_panic"
        }"#;
        let verdict = VerdictJson::parse(json).unwrap().unwrap();
        assert!(matches!(verdict.to_verdict(), Verdict::TestFailure { .. }));
    }
}
