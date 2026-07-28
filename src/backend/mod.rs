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

use std::fmt;

/// Verdict: the terminal classification of a run (plan §"Data models", "Verdict semantics").
///
/// Phase 0.5 implements only the minimal ladder:
/// - exit 0 -> Pass
/// - nonzero -> TestFailure
///
/// InfraFailure / gate_failure / cancelled / superseded classification arrives in
/// Phase 1a/2. Panics are allowed on unexpected backend output in the skeleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The remote ran the suite and it passed.
    Pass,
    /// The remote ran the suite and it failed (tests, compilation, etc.).
    TestFailure,
}

impl Verdict {
    /// Convert an exit code to a Verdict using the minimal ladder.
    ///
    /// Phase 0.5: 0 -> Pass, nonzero -> TestFailure.
    /// Phase 1a will expand this to include InfraFailure classification.
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Verdict::Pass,
            _ => Verdict::TestFailure,
        }
    }

    /// Convert the verdict to an exit code for the caller.
    ///
    /// Phase 0.5: Pass -> 0, TestFailure -> 1.
    /// Phase 1a will expand this to carry the actual test failure exit code.
    pub fn to_exit_code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::TestFailure => 1,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Pass => write!(f, "Pass"),
            Verdict::TestFailure => write!(f, "TestFailure"),
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

/// RunSpec: the specification of a run to submit to the remote backend.
///
/// Phase 0.5: minimal struct with repo URL, SHA, and args.
/// Phase 1a will expand this to include tool, subcommand, cwd_rel, and more.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    /// Repository URL (file:// for local testing in the skeleton).
    pub repo_url: String,
    /// Commit SHA to run (the content the executor checks out).
    pub sha: String,
    /// Arguments to pass to the command (e.g., ["test", "--", "--nocapture"]).
    pub args: Vec<String>,
}

impl RunSpec {
    /// Create a new RunSpec.
    pub fn new(repo_url: &str, sha: &str, args: Vec<String>) -> Self {
        RunSpec {
            repo_url: repo_url.to_string(),
            sha: sha.to_string(),
            args,
        }
    }
}

pub mod command;
