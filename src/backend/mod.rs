// gantry — RemoteBackend trait and Verdict enum (plan §"Phase 0.5", Components §5).
//
// Phase 1a: Full verdict ladder + verdict.json parsing (bf-23i).
//
// This module defines:
// - Verdict: the full verdict ladder (Pass/TestFailure/GateFailure/InfraFailure/Cancelled/Superseded)
// - FailureClass: detailed failure classification from verdict.json
// - VerdictJson: versioned verdict.json parsing structure
// - BackendError: minimal error type for backend operations
// - RunHandle: opaque handle returned by submit() and consumed by wait()
// - RemoteBackend trait: submit / stream_logs / wait / describe / cancel

use serde::{Deserialize, Serialize};
use std::fmt;

/// Verdict: the terminal classification of a run (plan §"Data models", "Verdict semantics").
///
/// Phase 1a implements the full verdict ladder:
/// - Pass: tests passed (exit 0)
/// - TestFailure: tests failed (exit non-zero, no local retry per INV-7)
/// - GateFailure: quality gate failed while tests passed (exit non-zero, no local retry)
/// - InfraFailure: no verdict produced (push/submit/schedule/stream/deadline) → local fallback
/// - Cancelled: user Ctrl-C (exit 130)
/// - Superseded: newer sha canceled this run (v1.x, exit 0)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The remote ran the suite and it passed.
    Pass,
    /// The remote ran the suite and it failed (tests, compilation, etc.).
    TestFailure,
    /// An enabled quality gate failed while tests passed (clippy, fmt, etc.).
    GateFailure,
    /// No verdict was produced - infra failure (push, submit, schedule, stream, deadline).
    /// Triggers capped local fallback. Never retried locally as test failure (INV-7).
    InfraFailure,
    /// User canceled the run (Ctrl-C).
    Cancelled,
    /// A newer sha superseded this run (v1.x).
    Superseded,
}

impl Verdict {
    /// Convert an exit code to a Verdict using the full ladder.
    ///
    /// Phase 1a: command contract is 0 -> Pass, 1 -> TestFailure, ≥2 -> InfraFailure.
    /// Argo backend reads verdict.json for authoritative classification.
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Verdict::Pass,
            1 => Verdict::TestFailure,
            _ => Verdict::InfraFailure,
        }
    }

    /// Convert the verdict to an exit code for the caller.
    ///
    /// Phase 1a: Pass/Superseded -> 0, TestFailure/GateFailure -> 1,
    /// InfraFailure -> 2 (signal for local fallback), Cancelled -> 130 (SIGINT).
    pub fn to_exit_code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::TestFailure => 1,
            Verdict::GateFailure => 1,
            Verdict::InfraFailure => 2,
            Verdict::Cancelled => 130,
            Verdict::Superseded => 0,
        }
    }

    /// Check if this verdict should trigger a local fallback (InfraFailure only).
    pub fn is_infra_failure(&self) -> bool {
        matches!(self, Verdict::InfraFailure)
    }

    /// Check if this verdict means the tests actually ran (Pass, TestFailure, GateFailure).
    pub fn has_test_result(&self) -> bool {
        matches!(
            self,
            Verdict::Pass | Verdict::TestFailure | Verdict::GateFailure
        )
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Pass => write!(f, "Pass"),
            Verdict::TestFailure => write!(f, "TestFailure"),
            Verdict::GateFailure => write!(f, "GateFailure"),
            Verdict::InfraFailure => write!(f, "InfraFailure"),
            Verdict::Cancelled => write!(f, "Cancelled"),
            Verdict::Superseded => write!(f, "Superseded"),
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
/// Phase 0.5: the handle is the stdout of the submit command (a string identifier
/// that the wait command can use to poll the run). The command backend's submit
/// argv emits this on stdout; the skeleton trusts it as an opaque handle.
///
/// Phase 1a will strengthen this into a structured type with backend-specific
/// state (e.g., Argo workflow name, pod UID, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct RunHandle {
    /// Opaque handle string (output of the submit command's stdout).
    pub handle: String,
}

impl RunHandle {
    /// Create a new RunHandle from a string.
    pub fn new(handle: &str) -> Self {
        RunHandle {
            handle: handle.to_string(),
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

/// FailureClass: detailed failure classification from verdict.json.
///
/// Derived from cargo's stable `--message-format json` stream in the remote
/// executor. Allows agents to branch on failure type without parsing logs.
///
/// Phase 1a: supports compile-error, test-failure, doctest, harness-panic.
/// Absent verdict.json = exit-code-only interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// Compilation error (cargo build/cargo check failed).
    CompileError,
    /// Test failure (cargo test found failing tests).
    TestFailure,
    /// Doctest failure.
    Doctest,
    /// Test harness panic (the test harness itself crashed).
    HarnessPanic,
}

/// VerdictJson: versioned verdict.json structure from remote executor.
///
/// Emitted as a Workflow output parameter by the remote template. Contains
/// the authoritative classification of the run outcome, including optional
/// failure class and infrastructure signals (OOM, deadline exceeded).
///
/// Consumers ignore unknown fields; absent verdict.json degrades gracefully
/// to exit-code-only interpretation (command contract).
///
/// Phase 1a: implements schema_version 1 with phase, exit_code, oom, deadline,
/// and optional failure_class. Later versions may add fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictJson {
    /// Schema version for backward compatibility.
    #[serde(rename = "schema_version")]
    pub schema_version: u32,

    /// Workflow phase from status.phase (Succeeded, Failed, Error, etc.).
    /// authoritative source for workflow-level outcome.
    pub phase: String,

    /// Exit code from the cargo/test run.
    pub exit_code: i32,

    /// Whether the pod was OOMKilled (InfraFailure signal).
    #[serde(default)]
    pub oom: bool,

    /// Whether the workflow exceeded its deadline (InfraFailure signal).
    #[serde(default)]
    pub deadline_exceeded: bool,

    /// Optional failure class from cargo --message-format json analysis.
    /// Absent means exit-code-only interpretation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
}

impl VerdictJson {
    /// Parse verdict.json from a JSON string.
    ///
    /// Returns Err if JSON is malformed or schema_version is unsupported.
    pub fn parse(json: &str) -> Result<Self, BackendError> {
        let parsed: Self = serde_json::from_str(json)
            .map_err(|e| BackendError::new(&format!("failed to parse verdict.json: {}", e)))?;

        // Validate schema version (Phase 1a only supports version 1)
        if parsed.schema_version != 1 {
            return Err(BackendError::new(&format!(
                "unsupported verdict.json schema version: {}",
                parsed.schema_version
            )));
        }

        Ok(parsed)
    }

    /// Convert the verdict.json to a Verdict using full ladder semantics.
    ///
    /// Applies infra-failure classification (OOM, deadline) before mapping
    /// exit code to TestFailure/Pass. Failure class is informational only.
    pub fn to_verdict(&self) -> Verdict {
        // InfraFailure signals take precedence
        if self.oom || self.deadline_exceeded {
            return Verdict::InfraFailure;
        }

        // Map exit code using standard ladder
        match self.exit_code {
            0 => Verdict::Pass,
            1 => Verdict::TestFailure,
            _ => Verdict::InfraFailure,
        }
    }
}

/// RunSpec: the specification of a run to submit to the remote backend.
///
/// Phase 0.5: minimal struct with repo URL, SHA, and args.
/// Phase 1a will expand this to include tool, subcommand, cwd_rel, and more.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    /// Tool to run (e.g., "cargo", "pytest").
    pub tool: String,
    /// Subcommand to invoke (e.g., "test", "build", "check").
    pub subcommand: String,
    /// Arguments to pass to the command (e.g., ["--", "--nocapture"]).
    pub args: Vec<String>,
    /// Repository URL (file:// for local testing in the skeleton).
    pub repo_url: String,
    /// Commit SHA to run (the content the executor checks out).
    pub sha: String,
    /// Working directory relative to repo root (e.g., "", "crates/foo").
    pub cwd_rel: String,
}

impl RunSpec {
    /// Create a new RunSpec.
    pub fn new(tool: &str, subcommand: &str, args: Vec<String>, repo_url: &str, sha: &str, cwd_rel: &str) -> Self {
        RunSpec {
            tool: tool.to_string(),
            subcommand: subcommand.to_string(),
            args,
            repo_url: repo_url.to_string(),
            sha: sha.to_string(),
            cwd_rel: cwd_rel.to_string(),
        }
    }
}

pub mod argo;
pub mod command;
