// gantry — RemoteBackend trait and Verdict enum (plan §"Phase 0.5", Components §5).
//
// Phase 1a: Full verdict ladder + verdict.json parsing (bf-23i).
//
// This module defines:
// - Verdict: the full verdict ladder (Pass/TestFailure/GateFailure/InfraFailure/Cancelled/Superseded)
// - BackendError: minimal error type for backend operations
// - RunHandle: opaque handle returned by submit() and consumed by wait()
// - RunStatus: coarse run-state query result (Pending/Running/Completed/Unknown)
// - RemoteBackend trait: submit / stream_logs / wait / describe / cancel / status
//
// The verdict.json parsing types (FailureClass, VerdictJson) are defined in
// [`crate::verdict`] — their single definition site — and re-exported here so
// call sites that predate the extraction compile unchanged.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub use crate::verdict::{FailureClass, VerdictJson};

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

    /// Interpret a finished run's outcome: verdict.json when the remote produced
    /// one, exit-code-only otherwise.
    ///
    /// This is the shared degradation contract (plan §"argo": "an absent
    /// verdict.json degrades gracefully to exit-code-only interpretation").
    /// `None` and an unparseable/mismatched-schema document both land on
    /// [`Verdict::from_exit_code`]; a document that parses is authoritative —
    /// its own exit code, infra signals, and failure class decide, even where
    /// they disagree with `exit_code`.
    pub fn interpret(exit_code: i32, verdict_json: Option<&str>) -> Self {
        match verdict_json.map(VerdictJson::parse) {
            Some(Ok(vj)) => vj.to_verdict(),
            _ => Verdict::from_exit_code(exit_code),
        }
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

/// RunStatus: coarse, non-terminal run state returned by status().
///
/// This is a point-in-time snapshot for polling, not a verdict — wait() remains
/// the authoritative source for the run's outcome. Unknown covers both states
/// a backend cannot determine (handle not recognized, query unsupported) so
/// callers can distinguish "still going" from "no answer".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// The run is submitted but has not started executing yet.
    Pending,
    /// The run is currently executing.
    Running,
    /// The run reached a terminal state (see wait() for the verdict).
    Completed,
    /// The backend cannot determine the run's state.
    Unknown,
}

/// RemoteBackend trait: the interface all remote executors must implement.
///
/// Phase 0.5: only submit() and wait() need real bodies; stream_logs, describe,
/// cancel, and status may panic. The command backend implements this trait using
/// hardcoded argv arrays that invoke a local bash executor.
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

    /// Query the current status of a run without waiting for it to complete.
    ///
    /// This is a best-effort point-in-time snapshot — unlike wait() it never
    /// blocks on the run and returns no verdict. Backends that cannot answer
    /// should report RunStatus::Unknown rather than error, so polling callers
    /// treat an unanswerable query as "no news" instead of a hard failure.
    ///
    /// Phase 0.5: may panic — this is not implemented in the skeleton.
    /// Later phases will map backend state onto RunStatus (Argo status.phase,
    /// command-backend process liveness).
    fn status(&self, h: &RunHandle) -> Result<RunStatus, BackendError> {
        let _ = h;
        panic!("status is not implemented in Phase 0.5");
    }
}

/// RunSpec: the specification of a run to submit to the remote backend.
///
/// All six fields are plan-mandated (plan §"Data models" → RunSpec). `cwd_rel`
/// is the caller's directory relative to the repo root; remote executors `cd`
/// into it before running the argv, so workspace-member invocations behave
/// identically remote and local.
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
    pub cwd_rel: PathBuf,
}

impl RunSpec {
    /// Create a new RunSpec.
    ///
    /// `cwd_rel` takes a string path relative to the repo root ("" = root) and
    /// is stored as a `PathBuf`, per plan §"Data models".
    pub fn new(
        tool: &str,
        subcommand: &str,
        args: Vec<String>,
        repo_url: &str,
        sha: &str,
        cwd_rel: &str,
    ) -> Self {
        RunSpec {
            tool: tool.to_string(),
            subcommand: subcommand.to_string(),
            args,
            repo_url: repo_url.to_string(),
            sha: sha.to_string(),
            cwd_rel: PathBuf::from(cwd_rel),
        }
    }
}

pub mod argo;
pub mod command;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant of the verdict ladder, so predicate tests can't silently
    /// miss one when a variant is added later.
    const ALL_VARIANTS: &[Verdict] = &[
        Verdict::Pass,
        Verdict::TestFailure,
        Verdict::GateFailure,
        Verdict::InfraFailure,
        Verdict::Cancelled,
        Verdict::Superseded,
    ];

    #[test]
    fn from_exit_code_follows_documented_ladder() {
        assert_eq!(Verdict::from_exit_code(0), Verdict::Pass);
        assert_eq!(Verdict::from_exit_code(1), Verdict::TestFailure);
        // >= 2 is InfraFailure, including signal-derived negative codes.
        assert_eq!(Verdict::from_exit_code(2), Verdict::InfraFailure);
        assert_eq!(Verdict::from_exit_code(3), Verdict::InfraFailure);
        assert_eq!(Verdict::from_exit_code(127), Verdict::InfraFailure);
        assert_eq!(Verdict::from_exit_code(-1), Verdict::InfraFailure);
    }

    #[test]
    fn to_exit_code_follows_documented_ladder() {
        assert_eq!(Verdict::Pass.to_exit_code(), 0);
        assert_eq!(Verdict::TestFailure.to_exit_code(), 1);
        assert_eq!(Verdict::GateFailure.to_exit_code(), 1);
        assert_eq!(Verdict::InfraFailure.to_exit_code(), 2);
        assert_eq!(Verdict::Cancelled.to_exit_code(), 130);
        assert_eq!(Verdict::Superseded.to_exit_code(), 0);
    }

    #[test]
    fn round_trip_preserves_semantics() {
        // from_exit_code(to_exit_code(v)) preserves semantics where the
        // exit-code ladder is defined. Superseded deliberately degrades to
        // Pass (both exit 0); Cancelled lands in the >=2 bucket as
        // InfraFailure (exit 130).
        for &v in ALL_VARIANTS {
            let round = Verdict::from_exit_code(v.to_exit_code());
            let expected = match v {
                Verdict::Pass | Verdict::Superseded => Verdict::Pass,
                Verdict::TestFailure | Verdict::GateFailure => Verdict::TestFailure,
                Verdict::InfraFailure | Verdict::Cancelled => Verdict::InfraFailure,
            };
            assert_eq!(round, expected, "round trip broke semantics for {}", v);
        }
    }

    #[test]
    fn is_infra_failure_true_only_for_infra_failure() {
        for &v in ALL_VARIANTS {
            assert_eq!(
                v.is_infra_failure(),
                v == Verdict::InfraFailure,
                "is_infra_failure wrong for {}",
                v
            );
        }
        assert!(Verdict::InfraFailure.is_infra_failure());
        assert!(!Verdict::Pass.is_infra_failure());
        assert!(!Verdict::TestFailure.is_infra_failure());
        assert!(!Verdict::Cancelled.is_infra_failure());
    }

    #[test]
    fn has_test_result_true_exactly_for_run_verdicts() {
        assert!(Verdict::Pass.has_test_result());
        assert!(Verdict::TestFailure.has_test_result());
        assert!(Verdict::GateFailure.has_test_result());
        assert!(!Verdict::InfraFailure.has_test_result());
        assert!(!Verdict::Cancelled.has_test_result());
        assert!(!Verdict::Superseded.has_test_result());

        // Belt-and-suspenders: exactly the three run verdicts, via the list.
        for &v in ALL_VARIANTS {
            let expected = matches!(
                v,
                Verdict::Pass | Verdict::TestFailure | Verdict::GateFailure
            );
            assert_eq!(
                v.has_test_result(),
                expected,
                "has_test_result wrong for {}",
                v
            );
        }
    }

    #[test]
    fn display_matches_variant_names_exactly() {
        // Display output is user-facing (banner, logs, close reasons), so the
        // spelling is contractual: assert each variant's exact string rather
        // than something loosely derived from Debug.
        let cases = [
            (Verdict::Pass, "Pass"),
            (Verdict::TestFailure, "TestFailure"),
            (Verdict::GateFailure, "GateFailure"),
            (Verdict::InfraFailure, "InfraFailure"),
            (Verdict::Cancelled, "Cancelled"),
            (Verdict::Superseded, "Superseded"),
        ];
        for (v, expected) in cases {
            assert_eq!(v.to_string(), expected, "Display for {}", v);
        }
    }

    #[test]
    fn run_handle_new_maps_the_handle_string() {
        let h = RunHandle::new("gantry-abc123");
        assert_eq!(h.handle, "gantry-abc123");
        // The handle is opaque — no mangling, trimming, or validation expected.
        let weird = RunHandle::new("  spaced/raw !handle ");
        assert_eq!(weird.handle, "  spaced/raw !handle ");
    }

    #[test]
    fn run_spec_new_maps_every_field() {
        let args = vec!["--".to_string(), "--nocapture".to_string()];
        let spec = RunSpec::new(
            "cargo",
            "test",
            args.clone(),
            "file:///tmp/repo",
            "0123456789abcdef",
            "crates/foo",
        );
        assert_eq!(spec.tool, "cargo");
        assert_eq!(spec.subcommand, "test");
        assert_eq!(spec.args, args);
        assert_eq!(spec.repo_url, "file:///tmp/repo");
        assert_eq!(spec.sha, "0123456789abcdef");
        assert_eq!(spec.cwd_rel, PathBuf::from("crates/foo"));
    }

    #[test]
    fn backend_error_new_maps_reason_and_impls_error() {
        let e = BackendError::new("workflow vanished");
        assert_eq!(e.reason, "workflow vanished");
        // Display delegates to the reason, so `e` formats as the bare message.
        assert_eq!(e.to_string(), "workflow vanished");
        // std::error::Error is a marker today; exercise it through the trait
        // object so the impl cannot silently disappear.
        let boxed: Box<dyn std::error::Error> = Box::new(e.clone());
        assert_eq!(boxed.to_string(), "workflow vanished");
    }

    /// A backend that does not override status() inherits the Phase-0.5 default
    /// body (same convention as stream_logs/describe/cancel) and keeps compiling
    /// unchanged until it implements the query. SkeletonBackend implements only
    /// the two required methods, so every default — status included — is the
    /// inherited body.
    struct SkeletonBackend;

    impl RemoteBackend for SkeletonBackend {
        fn submit(&self, _spec: &RunSpec) -> Result<RunHandle, BackendError> {
            Err(BackendError::new("skeleton backend submits nothing"))
        }

        fn wait(
            &self,
            _h: &RunHandle,
            _deadline: std::time::Instant,
        ) -> Result<Verdict, BackendError> {
            Err(BackendError::new("skeleton backend waits on nothing"))
        }
    }

    #[test]
    #[should_panic(expected = "status is not implemented in Phase 0.5")]
    fn default_status_follows_phase05_panic_convention() {
        let backend = SkeletonBackend;
        let _ = backend.status(&RunHandle::new("unused"));
    }

}
