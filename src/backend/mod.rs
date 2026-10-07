// gantry — RemoteBackend trait and Verdict enum (plan §"Phase 0.5", Components §5).
//
// Phase 1a: Full verdict ladder + verdict.json parsing (bf-23i).
//
// This module defines:
// - Verdict: the full verdict ladder (Pass/TestFailure/GateFailure/InfraFailure/Cancelled/Superseded)
// - BackendError: minimal error type for backend operations
// - BackendResult: Result alias used by every RemoteBackend method signature
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
/// Phase 1a: the structured `deadline_exceeded` flag (features.md v1.x
/// "timeout/deadline config per backend") lets the caller distinguish a run
/// that outlived its deadline from every other backend failure. Both are
/// InfraFailure — but a deadline expiry prints the "timed out, here's the
/// run URL" line instead of the generic wait-failure line.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendError {
    /// Human-readable reason for the error.
    pub reason: String,
    /// True when the error is a deadline expiry (the run's configured
    /// timeout elapsed before a verdict). Never true for spawn errors,
    /// parse errors, or remote test results.
    pub deadline_exceeded: bool,
    /// Where the abandoned run can still be watched — the backend's own run
    /// URL/identifier (the argo UI URL for the argo backend). Deadline
    /// expiry stops the watch but leaves the run alive on the remote, so
    /// the `[gantry] timeout` line appends this to point the operator at
    /// it. `None` on every other error, and on a backend with no watchable
    /// URL (the line then names the run by handle alone).
    pub run_url: Option<String>,
}

impl BackendError {
    /// Create a new BackendError with a reason.
    pub fn new(reason: &str) -> Self {
        BackendError {
            reason: reason.to_string(),
            deadline_exceeded: false,
            run_url: None,
        }
    }

    /// Create a deadline-expiry BackendError: the run outlived its
    /// configured timeout without producing a verdict.
    pub fn deadline(reason: &str) -> Self {
        BackendError {
            reason: reason.to_string(),
            deadline_exceeded: true,
            run_url: None,
        }
    }

    /// Create a deadline-expiry BackendError carrying the run URL: like
    /// [`BackendError::deadline`], plus where the abandoned run can still
    /// be watched (features.md v1.x "a clear timed-out, here's-the-run-URL
    /// message"). Expiry stops the watch; it does not cancel the run.
    pub fn deadline_with_url(reason: &str, run_url: &str) -> Self {
        BackendError {
            reason: reason.to_string(),
            deadline_exceeded: true,
            run_url: Some(run_url.to_string()),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for BackendError {}

/// BackendResult: the Result alias behind every RemoteBackend method.
///
/// Defined once so trait signatures, implementations, and call sites agree on
/// the error channel ([`BackendError`]) without re-spelling the full Result
/// type. Type aliases are transparent — an implementation may write the
/// expanded `Result<T, BackendError>` and still be a valid trait impl.
pub type BackendResult<T> = Result<T, BackendError>;

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
    fn submit(&self, spec: &RunSpec) -> BackendResult<RunHandle>;

    /// Stream logs from the remote run to a writer (best-effort).
    ///
    /// Phase 0.5: may panic — this is not implemented in the skeleton.
    /// Phase 1a will implement this for both command and Argo backends.
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn std::io::Write) -> BackendResult<()> {
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
    fn wait(&self, h: &RunHandle, deadline: std::time::Instant) -> BackendResult<Verdict>;

    /// Wait for the remote run to complete and return its outcome: the
    /// verdict plus the failure class the remote attributed to it (plan
    /// §Component 5 failure taxonomy, verdict.json v2).
    ///
    /// This is what the client half of the taxonomy records into runs.jsonl
    /// ([`crate::runlog::VerdictRecord::failure_class`]): a remote run that
    /// died carries *what it died of* alongside *that it died*. The class is
    /// `None` for every shape of "not known from a parsed document" — no
    /// usable verdict.json (absent, malformed, newer schema), a backend that
    /// never reads documents at all — so the runlog's field contract (null
    /// for uninstrumented producers) falls out of the plumbing instead of
    /// being enforced per call site. Whether a threaded class belongs on the
    /// record for the verdict it arrived with is the runlog contract's call,
    /// applied where the record is built.
    ///
    /// Default: [`Self::wait`]'s verdict with no class. Backends that parse
    /// verdict.json override this; every other implementation — and every
    /// consumer written against the plain ladder — keeps compiling unchanged.
    fn wait_outcome(
        &self,
        h: &RunHandle,
        deadline: std::time::Instant,
    ) -> BackendResult<(Verdict, Option<FailureClass>)> {
        self.wait(h, deadline).map(|verdict| (verdict, None))
    }

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
    fn cancel(&self, h: &RunHandle) -> BackendResult<()> {
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
    fn status(&self, h: &RunHandle) -> BackendResult<RunStatus> {
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

    #[test]
    fn backend_error_deadline_constructors_flag_expiry_and_carry_the_url() {
        // Only the expiry constructors raise the flag — a spawn error or a
        // parse error must never print the timeout line — and the run URL
        // rides only the constructor built for it: the plain expiry keeps
        // the bare timeout line (the command backend's contract), the
        // with-url expiry hands the operator something to watch.
        let plain = BackendError::new("pod vanished");
        assert!(!plain.deadline_exceeded);
        assert_eq!(plain.run_url, None);

        let expiry = BackendError::deadline("run h-1 deadline exceeded");
        assert!(expiry.deadline_exceeded);
        assert_eq!(expiry.run_url, None);

        let with_url = BackendError::deadline_with_url(
            "run h-2 deadline exceeded",
            "https://argo.example.com/workflows/ns/h-2",
        );
        assert!(with_url.deadline_exceeded);
        assert_eq!(
            with_url.run_url.as_deref(),
            Some("https://argo.example.com/workflows/ns/h-2")
        );
    }

    /// A backend that does not override status() inherits the Phase-0.5 default
    /// body (same convention as stream_logs/describe/cancel) and keeps compiling
    /// unchanged until it implements the query. SkeletonBackend implements only
    /// the two required methods, so every default — status included — is the
    /// inherited body.
    struct SkeletonBackend;

    impl RemoteBackend for SkeletonBackend {
        fn submit(&self, _spec: &RunSpec) -> BackendResult<RunHandle> {
            Err(BackendError::new("skeleton backend submits nothing"))
        }

        fn wait(&self, _h: &RunHandle, _deadline: std::time::Instant) -> BackendResult<Verdict> {
            Err(BackendError::new("skeleton backend waits on nothing"))
        }
    }

    #[test]
    #[should_panic(expected = "status is not implemented in Phase 0.5")]
    fn default_status_follows_phase05_panic_convention() {
        let backend = SkeletonBackend;
        let _ = backend.status(&RunHandle::new("unused"));
    }

    /// A stub that answers from the handle string alone: submit() encodes the
    /// spec's sha into the handle, and status()/wait() consume it. Holds no
    /// state and touches no filesystem or environment, so the tests below are
    /// safe under parallel execution.
    struct StubBackend;

    impl StubBackend {
        fn run_spec(sha: &str) -> RunSpec {
            RunSpec::new("cargo", "test", vec![], "file:///tmp/gantry-stub", sha, "")
        }

        fn deadline_ahead() -> std::time::Instant {
            std::time::Instant::now() + std::time::Duration::from_secs(60)
        }
    }

    impl RemoteBackend for StubBackend {
        fn submit(&self, spec: &RunSpec) -> BackendResult<RunHandle> {
            Ok(RunHandle::new(&format!("stub-{}", spec.sha)))
        }

        fn status(&self, h: &RunHandle) -> BackendResult<RunStatus> {
            match h.handle.as_str() {
                "stub-pass" | "stub-fail" => Ok(RunStatus::Running),
                other => Err(BackendError::new(&format!("unknown handle {other:?}"))),
            }
        }

        fn wait(&self, h: &RunHandle, deadline: std::time::Instant) -> BackendResult<Verdict> {
            if std::time::Instant::now() > deadline {
                return Err(BackendError::new("deadline passed"));
            }
            match h.handle.as_str() {
                "stub-pass" => Ok(Verdict::Pass),
                "stub-fail" => Ok(Verdict::TestFailure),
                other => Err(BackendError::new(&format!("unknown handle {other:?}"))),
            }
        }
    }

    #[test]
    fn backend_result_alias_is_transparent_with_result() {
        // Compile-level contract: a BackendResult flows into and out of
        // positions expecting the spelled-out Result type unchanged.
        fn take_result(r: Result<Verdict, BackendError>) -> BackendResult<Verdict> {
            r
        }
        let alias: BackendResult<Verdict> = Ok(Verdict::Pass);
        assert_eq!(take_result(alias), Ok(Verdict::Pass));

        let err: BackendResult<Verdict> = Err(BackendError::new("no run"));
        assert_eq!(
            take_result(err).err().map(|e| e.reason),
            Some("no run".to_string())
        );
    }

    #[test]
    fn dyn_backend_submit_returns_a_run_handle() {
        let backend: &dyn RemoteBackend = &StubBackend;
        let h = backend
            .submit(&StubBackend::run_spec("pass"))
            .expect("stub submit succeeds");
        assert_eq!(
            h.handle, "stub-pass",
            "handle carries the submitted sha for status()/wait() to consume"
        );
    }

    #[test]
    fn dyn_backend_status_consumes_the_handle() {
        let backend: Box<dyn RemoteBackend> = Box::new(StubBackend);
        let h = backend
            .submit(&StubBackend::run_spec("pass"))
            .expect("submit");
        assert_eq!(
            backend.status(&h).expect("status"),
            RunStatus::Running,
            "submitted run reports Running"
        );

        // A handle the backend did not issue is an Err, not a fabricated state.
        let err = backend
            .status(&RunHandle::new("stub-never-submitted"))
            .expect_err("unknown handle errors");
        assert!(
            err.reason.contains("unknown handle"),
            "unexpected error: {}",
            err.reason
        );
    }

    #[test]
    fn dyn_backend_wait_consumes_the_handle_and_yields_the_verdict() {
        let backend: &dyn RemoteBackend = &StubBackend;
        let deadline = StubBackend::deadline_ahead();

        let h = backend
            .submit(&StubBackend::run_spec("pass"))
            .expect("submit");
        assert_eq!(backend.wait(&h, deadline).expect("wait"), Verdict::Pass);

        let h = backend
            .submit(&StubBackend::run_spec("fail"))
            .expect("submit");
        assert_eq!(
            backend.wait(&h, deadline).expect("wait"),
            Verdict::TestFailure
        );
    }

    #[test]
    fn dyn_backend_wait_honours_an_elapsed_deadline() {
        let backend: &dyn RemoteBackend = &StubBackend;
        let h = backend
            .submit(&StubBackend::run_spec("pass"))
            .expect("submit");
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let err = backend.wait(&h, past).expect_err("elapsed deadline errors");
        assert_eq!(err.reason, "deadline passed");
    }
}
