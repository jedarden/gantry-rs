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
// - RunStatus: coarse run-state query result (Pending/Running/Completed/Unknown)
// - RemoteBackend trait: submit / stream_logs / wait / describe / cancel / status

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

/// FailureClass: detailed failure classification from verdict.json.
///
/// Derived from cargo's stable `--message-format json` stream in the remote
/// executor. Allows agents to branch on failure type without parsing logs.
///
/// Phase 1a: supports compile-error, test-failure, doctest, harness-panic, gate-failure.
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
    /// Quality gate failure (clippy, fmt, etc.) while tests passed.
    GateFailure,
}

impl FailureClass {
    /// Parse a verdict.json `failure_class` string.
    ///
    /// Mirrors the derived kebab-case serde names exactly — pinned by
    /// [`failure_class_derived_names_match_from_kebab`] so the two paths
    /// cannot drift. Unknown values return None (schema evolution: a class
    /// coined by a newer producer must not poison the whole document —
    /// see [`VerdictJson`] for why).
    pub fn from_kebab(s: &str) -> Option<Self> {
        match s {
            "compile-error" => Some(FailureClass::CompileError),
            "test-failure" => Some(FailureClass::TestFailure),
            "doctest" => Some(FailureClass::Doctest),
            "harness-panic" => Some(FailureClass::HarnessPanic),
            "gate-failure" => Some(FailureClass::GateFailure),
            _ => None,
        }
    }
}

/// Deserialize `failure_class` leniently: an unrecognized class string (or
/// null) reads as absent instead of failing the whole verdict.json parse.
///
/// The core signals in the document (oom, deadline_exceeded, exit_code) must
/// survive a producer adding a class this version doesn't know — dropping just
/// the class keeps an OOM run classifying as InfraFailure rather than
/// misreading it as a test failure.
fn deserialize_lenient_failure_class<'de, D>(
    deserializer: D,
) -> Result<Option<FailureClass>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.as_deref().and_then(FailureClass::from_kebab))
}

/// VerdictJson: versioned verdict.json structure from remote executor.
///
/// Emitted as a Workflow output parameter by the remote template. Contains
/// the authoritative classification of the run outcome, including optional
/// failure class and infrastructure signals (OOM, deadline exceeded).
///
/// Consumers ignore unknown fields; absent verdict.json degrades gracefully
/// to exit-code-only interpretation (command contract) — see
/// [`Verdict::interpret`]. An unrecognized `failure_class` string likewise
/// reads as absent rather than failing the parse, so a producer newer than
/// this consumer can never cost the document its infra signals.
///
/// Phase 1a: implements schema_version 1 with phase, exit_code, oom, deadline,
/// and optional failure_class. Later versions may add fields; a
/// schema_version this parser does not know is a loud parse error (and thus
/// an exit-code-only degradation), never a guess.
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
    /// Absent (or unrecognized) means exit-code-only interpretation.
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_failure_class",
        skip_serializing_if = "Option::is_none"
    )]
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

    /// Check if this verdict represents a gate failure.
    ///
    /// Gate failures occur when quality gates (clippy, fmt, etc.) fail
    /// while the actual test suite passed. The remote contract attributes
    /// these explicitly via failure_class; without that attribution the
    /// exit-code-only interpretation stays conservative (test failure).
    fn is_gate_failure(&self) -> bool {
        matches!(self.failure_class, Some(FailureClass::GateFailure))
    }

    /// Convert the verdict.json to a Verdict using full ladder semantics.
    ///
    /// Precedence: infrastructure signals (OOMKilled, deadline exceeded, the
    /// workflow itself erroring) classify as InfraFailure first — a cap firing
    /// says nothing about the code, so it must never read as a test result —
    /// then explicit gate attribution, then the exit-code ladder. An absent
    /// verdict.json never reaches this method; it degrades to exit-code-only
    /// via [`Verdict::interpret`].
    pub fn to_verdict(&self) -> Verdict {
        // InfraFailure signals take precedence (OOM, deadline, workflow Error)
        if self.oom || self.deadline_exceeded || self.phase == "Error" {
            return Verdict::InfraFailure;
        }

        // Gate failure detection (quality gate failed while tests passed)
        if self.is_gate_failure() {
            return Verdict::GateFailure;
        }

        // Standard exit code mapping for test failures and pass
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
            cwd_rel: cwd_rel.to_string(),
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
        assert_eq!(spec.cwd_rel, "crates/foo");
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
    /// unchanged until it implements the query.
    #[test]
    #[should_panic(expected = "status is not implemented in Phase 0.5")]
    fn default_status_follows_phase05_panic_convention() {
        let backend = crate::backend::command::CommandBackend::new();
        let _ = backend.status(&RunHandle::new("unused"));
    }

    // --- verdict.json parsing: failure classes ------------------------------

    /// Build a schema-1 verdict.json document with the given fields
    /// (`failure_class` omitted when None).
    fn verdict_doc(
        phase: &str,
        exit_code: i32,
        oom: bool,
        deadline: bool,
        class: Option<&str>,
    ) -> String {
        let mut obj = serde_json::json!({
            "schema_version": 1,
            "phase": phase,
            "exit_code": exit_code,
            "oom": oom,
            "deadline_exceeded": deadline,
        });
        if let Some(class) = class {
            obj["failure_class"] = serde_json::Value::String(class.to_string());
        }
        obj.to_string()
    }

    #[test]
    fn failure_class_strings_parse_to_variants() {
        let cases = [
            ("compile-error", FailureClass::CompileError),
            ("test-failure", FailureClass::TestFailure),
            ("doctest", FailureClass::Doctest),
            ("harness-panic", FailureClass::HarnessPanic),
            ("gate-failure", FailureClass::GateFailure),
        ];
        for (raw, expected) in cases {
            let vj = VerdictJson::parse(&verdict_doc("Failed", 1, false, false, Some(raw)))
                .unwrap_or_else(|e| panic!("{raw} must parse: {e}"));
            assert_eq!(vj.failure_class, Some(expected), "class {raw:?}");
        }
    }

    /// The lenient parser and the derived serde names must accept exactly the
    /// same strings — this is the guard against the two paths drifting.
    #[test]
    fn failure_class_derived_names_match_from_kebab() {
        for class in [
            FailureClass::CompileError,
            FailureClass::TestFailure,
            FailureClass::Doctest,
            FailureClass::HarnessPanic,
            FailureClass::GateFailure,
        ] {
            let serialized = serde_json::to_string(&class).expect("serialize");
            let raw = serialized.trim_matches('"');
            assert_eq!(FailureClass::from_kebab(raw), Some(class), "class {raw:?}");
        }
    }

    /// A failure_class a newer producer coined reads as absent — the document
    /// still parses and the exit-code ladder still classifies the run.
    #[test]
    fn unknown_failure_class_degrades_to_absent_not_error() {
        let vj = VerdictJson::parse(&verdict_doc(
            "Failed",
            1,
            false,
            false,
            Some("benchmark-regression"),
        ))
        .expect("unknown class must not fail the parse");
        assert_eq!(vj.failure_class, None);
        assert_eq!(vj.to_verdict(), Verdict::TestFailure);
    }

    /// The reason leniency exists: an OOM run whose verdict.json also carries
    /// an unrecognized class must still classify as InfraFailure, not get its
    /// infra signals thrown away with a strict-parse error.
    #[test]
    fn unknown_failure_class_keeps_infra_signals() {
        let vj = VerdictJson::parse(&verdict_doc(
            "Failed",
            137,
            true,
            false,
            Some("benchmark-regression"),
        ))
        .expect("unknown class must not fail the parse");
        assert_eq!(vj.to_verdict(), Verdict::InfraFailure);
    }

    #[test]
    fn null_failure_class_reads_as_absent() {
        let doc = r#"{
            "schema_version": 1,
            "phase": "Succeeded",
            "exit_code": 0,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": null
        }"#;
        let vj = VerdictJson::parse(doc).expect("null class must parse");
        assert_eq!(vj.failure_class, None);
        assert_eq!(vj.to_verdict(), Verdict::Pass);
    }

    // --- verdict.json parsing: schema and defaults --------------------------

    /// Optional fields are truly optional: the minimal schema-1 document
    /// parses with clean defaults.
    #[test]
    fn absent_optional_fields_default_cleanly() {
        let doc = r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0}"#;
        let vj = VerdictJson::parse(doc).expect("minimal document must parse");
        assert!(!vj.oom);
        assert!(!vj.deadline_exceeded);
        assert_eq!(vj.failure_class, None);
        assert_eq!(vj.to_verdict(), Verdict::Pass);
    }

    /// Unknown fields from a future schema version are ignored, wherever they
    /// appear in the document.
    #[test]
    fn verdict_json_unknown_fields_ignored() {
        let doc = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "test-failure",
            "toolchain": "1.96.0",
            "node": {"name": "runner-xyz", "resources": {"cpu": 4}}
        }"#;
        let vj = VerdictJson::parse(doc).expect("unknown fields must be ignored");
        assert_eq!(vj.failure_class, Some(FailureClass::TestFailure));
        assert_eq!(vj.to_verdict(), Verdict::TestFailure);
    }

    // --- verdict ladder semantics -------------------------------------------

    /// Deadline-exceeded workflows are InfraFailure by definition: the cap
    /// firing says nothing about the code (plan §"Verdict semantics").
    #[test]
    fn deadline_exceeded_is_infra_failure() {
        let vj = VerdictJson::parse(&verdict_doc("Failed", 1, false, true, None))
            .expect("deadline document must parse");
        assert_eq!(vj.to_verdict(), Verdict::InfraFailure);
    }

    /// The workflow itself erroring (template not found, controller break) is
    /// infra even when an exit code claims a pass — no suite ever ran.
    #[test]
    fn workflow_error_phase_is_infra_failure() {
        let vj = VerdictJson::parse(&verdict_doc("Error", 0, false, false, None))
            .expect("Error-phase document must parse");
        assert_eq!(vj.to_verdict(), Verdict::InfraFailure);
        assert!(vj.to_verdict().is_infra_failure());
    }

    /// Infra signals outrank explicit gate attribution: a capped run has no
    /// test result for a gate to be attributed to.
    #[test]
    fn oom_outranks_gate_attribution() {
        let vj = VerdictJson::parse(&verdict_doc("Failed", 1, true, false, Some("gate-failure")))
            .expect("document must parse");
        assert_eq!(vj.to_verdict(), Verdict::InfraFailure);
    }

    /// Gate attribution outranks the exit-code ladder: the canonical gate
    /// failure is "tests passed (exit 0 of the suite), clippy did not".
    #[test]
    fn gate_attribution_outranks_exit_code() {
        let vj = VerdictJson::parse(&verdict_doc(
            "Succeeded",
            0,
            false,
            false,
            Some("gate-failure"),
        ))
        .expect("document must parse");
        let verdict = vj.to_verdict();
        assert_eq!(verdict, Verdict::GateFailure);
        assert!(!verdict.is_infra_failure());
        assert!(verdict.has_test_result());
    }

    // --- the degradation contract (absent verdict.json) ---------------------

    /// Absent verdict.json degrades to exit-code-only interpretation.
    #[test]
    fn interpret_absent_document_uses_exit_code_only() {
        assert_eq!(Verdict::interpret(0, None), Verdict::Pass);
        assert_eq!(Verdict::interpret(1, None), Verdict::TestFailure);
        assert_eq!(Verdict::interpret(137, None), Verdict::InfraFailure);
        assert_eq!(Verdict::interpret(-1, None), Verdict::InfraFailure);
    }

    /// A parseable document is authoritative even where it disagrees with the
    /// caller-side exit code.
    #[test]
    fn interpret_parseable_document_is_authoritative() {
        // Document says OOM; a caller-side exit code of 1 would say test
        // failure — the document wins.
        let doc = verdict_doc("Failed", 137, true, false, None);
        assert_eq!(Verdict::interpret(1, Some(&doc)), Verdict::InfraFailure);

        // Gate attribution the exit-code ladder cannot express.
        let doc = verdict_doc("Succeeded", 0, false, false, Some("gate-failure"));
        assert_eq!(Verdict::interpret(0, Some(&doc)), Verdict::GateFailure);
    }

    /// Malformed JSON and a schema_version this parser does not know both
    /// degrade to exit-code-only rather than guessing.
    #[test]
    fn interpret_malformed_or_mismatched_document_degrades_to_exit_code() {
        assert_eq!(
            Verdict::interpret(1, Some("{not json")),
            Verdict::TestFailure
        );
        assert_eq!(Verdict::interpret(0, Some("")), Verdict::Pass);

        let future = r#"{
            "schema_version": 2,
            "phase": "Succeeded",
            "exit_code": 0,
            "ladder": "v2"
        }"#;
        assert_eq!(Verdict::interpret(0, Some(future)), Verdict::Pass);
        assert_eq!(Verdict::interpret(1, Some(future)), Verdict::TestFailure);
    }

    // --- property tests ------------------------------------------------------
    //
    // Dependency-free property tests over a deterministic generated space:
    // every base document in the full field matrix crossed with every unknown-
    // field injection. If a new field or value shape breaks any of these, the
    // property names the exact combination.

    /// The full field matrix of schema-1 documents, as (phase, exit_code, oom,
    /// deadline, failure_class) tuples.
    fn all_schema_one_documents() -> Vec<String> {
        let classes = [
            None,
            Some("compile-error"),
            Some("test-failure"),
            Some("doctest"),
            Some("harness-panic"),
            Some("gate-failure"),
        ];
        let mut docs = Vec::new();
        for phase in ["Succeeded", "Failed", "Error"] {
            for exit_code in [0, 1, 2, 137, -1] {
                for oom in [false, true] {
                    for deadline in [false, true] {
                        for class in classes {
                            docs.push(verdict_doc(phase, exit_code, oom, deadline, class));
                        }
                    }
                }
            }
        }
        docs
    }

    /// Return `base` with extra top-level fields injected.
    fn with_fields(base: &str, fields: &[(&str, serde_json::Value)]) -> String {
        let mut value: serde_json::Value = serde_json::from_str(base).expect("base is valid JSON");
        let obj = value.as_object_mut().expect("base is an object");
        for (name, v) in fields {
            obj.insert((*name).to_string(), v.clone());
        }
        value.to_string()
    }

    /// The value shapes a v2+ producer might attach to a future field.
    fn unknown_field_values() -> Vec<serde_json::Value> {
        vec![
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!(3.5),
            serde_json::json!("text"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({"nested": {"a": 1}}),
        ]
    }

    /// Property: schema evolution tolerance. For every base document and every
    /// unknown-field injection (the names and value shapes a v2+ producer
    /// might add), parsing must yield exactly what the base yields and the
    /// verdict must be unchanged; the caller-side exit code must never
    /// override a parseable document.
    #[test]
    fn property_unknown_fields_never_change_parse_or_verdict() {
        const NAMES: &[&str] = &["future_field", "contract_version", "toolchain", "node_name"];
        let values = unknown_field_values();

        for base in all_schema_one_documents() {
            let expected = VerdictJson::parse(&base).expect("base document parses");
            let expected_verdict = expected.to_verdict();

            for name in NAMES {
                for value in &values {
                    let mutated = with_fields(&base, &[(*name, value.clone())]);
                    let parsed = VerdictJson::parse(&mutated)
                        .unwrap_or_else(|e| panic!("{mutated}: unknown field broke parse: {e}"));
                    assert_eq!(parsed, expected, "unknown field {name} changed the parse");
                    assert_eq!(
                        parsed.to_verdict(),
                        expected_verdict,
                        "unknown field {name} changed the verdict"
                    );
                    assert_eq!(
                        Verdict::interpret(0, Some(&mutated)),
                        expected_verdict,
                        "caller exit code must not override a parseable document"
                    );
                }
            }

            // All unknown fields at once.
            let all: Vec<(&str, serde_json::Value)> = NAMES
                .iter()
                .zip(values.iter())
                .map(|(name, value)| (*name, value.clone()))
                .collect();
            let mutated = with_fields(&base, &all);
            let parsed = VerdictJson::parse(&mutated)
                .unwrap_or_else(|e| panic!("{mutated}: combined fields broke parse: {e}"));
            assert_eq!(
                parsed, expected,
                "combined unknown fields changed the parse"
            );
            assert_eq!(parsed.to_verdict(), expected_verdict);
        }
    }

    /// Property: serialize → parse round-trips to the same document and the
    /// same verdict, for every document in the field matrix.
    #[test]
    fn property_serialization_round_trip_preserves_verdict() {
        for base in all_schema_one_documents() {
            let parsed = VerdictJson::parse(&base).expect("base document parses");
            let serialized = serde_json::to_string(&parsed).expect("serialize");

            // The serialized form must not carry absent optionals (skip_serializing_if).
            if parsed.failure_class.is_none() {
                assert!(
                    !serialized.contains("failure_class"),
                    "absent failure_class must not serialize: {serialized}"
                );
            }

            let reparsed = VerdictJson::parse(&serialized)
                .unwrap_or_else(|e| panic!("{serialized}: round trip broke parse: {e}"));
            assert_eq!(reparsed, parsed, "round trip changed the document");
            assert_eq!(reparsed.to_verdict(), parsed.to_verdict());
        }
    }

    /// Property: every schema_version this parser does not know is rejected
    /// loudly (which degrades callers to exit-code-only), never silently
    /// interpreted as version 1.
    #[test]
    fn property_unsupported_schema_versions_are_rejected() {
        for version in [0u32, 2, 3, 42, u32::MAX] {
            let doc =
                format!(r#"{{"schema_version": {version}, "phase": "Succeeded", "exit_code": 0}}"#);
            let err = VerdictJson::parse(&doc)
                .expect_err(&format!("schema_version {version} must be rejected"));
            assert!(
                err.reason.contains("schema version"),
                "{version}: wrong error: {}",
                err.reason
            );
        }
    }

    /// Property: the exit-code ladder is total and consistent — every i32 in
    /// the sweep maps to a verdict whose round-trip through to_exit_code
    /// preserves the mapped class, and the absent-verdict.json degradation
    /// agrees with from_exit_code everywhere.
    #[test]
    fn property_exit_code_ladder_is_total_and_consistent() {
        for code in -500..=500 {
            let verdict = Verdict::from_exit_code(code);
            let expected = match code {
                0 => Verdict::Pass,
                1 => Verdict::TestFailure,
                _ => Verdict::InfraFailure,
            };
            assert_eq!(verdict, expected, "code {code}");

            let round = verdict.to_exit_code();
            assert_eq!(
                Verdict::from_exit_code(round),
                expected,
                "round trip broke code {code}"
            );

            assert_eq!(
                Verdict::interpret(code, None),
                expected,
                "degradation broke code {code}"
            );
        }
    }
}
