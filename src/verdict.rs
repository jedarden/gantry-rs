// gantry — verdict.json parsing types (plan §"Data models", "Verdict semantics").
//
// Phase 1a: verdict.json parsing (bf-23i); extracted from `backend` (bf-2jnj)
// so this module is the single definition site for the verdict.json schema.
//
// This module defines:
// - FailureClass: detailed failure classification from verdict.json
// - VerdictJson: versioned verdict.json parsing structure
//
// Both are re-exported from [`crate::backend`] so call sites that predate the
// extraction (src/backend/argo.rs, src/decision.rs) compile unchanged.

use serde::{Deserialize, Serialize};

use crate::backend::{BackendError, Verdict};

/// The verdict.json schema versions this build parses (plan §"Versioning &
/// compatibility"). Version 1 is the pre-taxonomy producer: `failure_class`
/// was an optional refinement a template chose to emit (in practice, only
/// for gate attribution). Version 2 is the failure-taxonomy contract: a
/// producer that instruments the suite with `--message-format json` derives
/// the class for every suite that ran to completion and failed, not just for
/// gates. The shape is identical — v2 is a semantics bump, which is exactly
/// what the version field exists to carry — and both parse the same here.
///
/// A version outside this list is a loud parse error (never silently read as
/// a known one) and degrades the caller to exit-code-only.
const SUPPORTED_SCHEMA_VERSIONS: [u32; 2] = [1, 2];

/// The schema_version this build fabricates when it must construct a document
/// itself ([`VerdictJson::from_exit_code`]): the current version.
const CURRENT_SCHEMA_VERSION: u32 = 2;

/// The remote contract version this client speaks (plan §"Versioning &
/// compatibility", "Remote contract").
///
/// The Argo backend sends it as the Workflow's `contract-version` parameter
/// (src/backend/argo.rs `Workflow::new`); the contrib template echoes it back
/// verbatim in verdict.json. A document that echoes a *different* version
/// claims a contract this client cannot interpret — that is contract drift,
/// and it classifies as [`Verdict::InfraFailure`] (never a misread verdict).
/// An *absent* echo is not drift: the field is an additive-evolution
/// addition, so a schema-1 producer that predates the handshake simply omits
/// it, and this client knows the schema-1 document completely.
pub const CONTRACT_VERSION: &str = "1";

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

    /// Classify a failed suite from cargo's `--message-format json` stream —
    /// the reference implementation of the remote failure taxonomy
    /// (ideas-ledger finalist 7, adopted 2026-07-22; plan §Component 5
    /// "verdict.json v2 failure taxonomy").
    ///
    /// This is the normative form of the algorithm the reference producer
    /// (`contrib/argo/gantry-verify-workflowtemplate.yml`) executes in jq/awk
    /// when it stamps `failure_class` into verdict.json; the shared fixture
    /// corpus (`tests/fixtures/failure-class-corpus.json`) pins both to the
    /// same semantics — the producer-parity test below executes the
    /// template's own snippet on every fixture, so drift fails `cargo test`
    /// instead of surfacing as an agent branching on a class the run
    /// contradicts. Inputs are exactly what the producer has:
    ///
    /// - `exit_code` — the *raw* cargo exit code (0–255 shell domain), not
    ///   the client-ladder code verdict.json carries.
    /// - `instrumented` — false when the caller chose their own
    ///   `--message-format`, in which case the raw protocol was never
    ///   captured and no class is derivable (instrumentation must not fight
    ///   the argv).
    /// - `messages` — the archived cargo JSON protocol stream (one JSON
    ///   object per line, mixed with the harness's human lines that share
    ///   stdout). Read line-wise and leniently on purpose: `jq -s` would
    ///   need the whole file to parse, and it never does.
    /// - `run_log` — the human-visible run output (rendered diagnostics and
    ///   harness lines), which doctest and harness-panic detection read.
    ///
    /// Returns `None` outside the classifiable window: a passing suite (exit
    /// 0) has no failure to name, a signal-killed suite (≥128) is infra and
    /// is classified by the verdict ladder, not a failure class, and an
    /// uninstrumented run has no protocol stream. `GateFailure` is never
    /// derived here — gates run only after a passing suite and are
    /// attributed explicitly by the producer, never read off the stream.
    ///
    /// Detection order mirrors the producer exactly:
    /// 1. **compile-error** — any protocol line is a compiler-message whose
    ///    diagnostic level is `error` (rustc never finished; outranks
    ///    everything a later stage printed).
    /// 2. **harness-panic** — the run log reports a stack overflow (a test
    ///    binary crashed the harness).
    /// 3. **doctest** — the only `test result: FAILED` lines are the ones
    ///    following a `Doc-tests` section header.
    /// 4. **test-failure** — everything else.
    pub fn classify(
        exit_code: i32,
        instrumented: bool,
        messages: &str,
        run_log: &str,
    ) -> Option<Self> {
        // The classifiable window: a suite that ran to completion and failed.
        // (The producer additionally never reads the class it derives for
        // exit 127 — command-not-found is intercepted as infra downstream —
        // but the window predicate itself is `!= 0 && < 128` on both sides.)
        if exit_code == 0 || exit_code >= 128 || !instrumented {
            return None;
        }

        // 1. compile-error: a compiler-message diagnostic at level `error`.
        //    Lenient per line — a line that is not a JSON object (the
        //    harness's human output shares the stream) is skipped, not an
        //    error, so one garbled line cannot blind the classifier.
        let compile_error = messages.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .filter(|value| value.is_object())
                .is_some_and(|value| {
                    value.get("reason").and_then(|r| r.as_str()) == Some("compiler-message")
                        && value
                            .get("message")
                            .and_then(|m| m.get("level"))
                            .and_then(|l| l.as_str())
                            == Some("error")
                })
        });
        if compile_error {
            return Some(FailureClass::CompileError);
        }

        // 2. harness-panic: a test binary overflowed its stack.
        if run_log.contains("has overflowed its stack") {
            return Some(FailureClass::HarnessPanic);
        }

        // 3. doctest: every `test result: FAILED` line comes after a
        //    `Doc-tests` section header — the lib/integration sections print
        //    theirs first, so a failed unit test sets `other_failed` and the
        //    run stays a plain test failure.
        let mut docs_seen = false;
        let mut doc_failed = false;
        let mut other_failed = false;
        for line in run_log.lines() {
            if line.contains("Doc-tests") {
                docs_seen = true;
            }
            if line.starts_with("test result: FAILED") {
                if docs_seen {
                    doc_failed = true;
                } else {
                    other_failed = true;
                }
            }
        }
        if doc_failed && !other_failed {
            return Some(FailureClass::Doctest);
        }

        // 4. test-failure: the fallthrough class.
        Some(FailureClass::TestFailure)
    }
}

/// Deserialize `failure_class` leniently: an unrecognized class string (or
/// null) reads as absent instead of failing the whole document parse.
///
/// The core signals in the document (oom, deadline_exceeded, exit_code) must
/// survive a producer adding a class this version doesn't know — dropping just
/// the class keeps an OOM run classifying as InfraFailure rather than
/// misreading it as a test failure.
///
/// `pub(crate)` because the runs.jsonl verdict record carries the same class
/// under the same leniency contract (src/runlog.rs `VerdictRecord`): one
/// deserializer, one semantics, two documents.
pub(crate) fn deserialize_lenient_failure_class<'de, D>(
    deserializer: D,
) -> Result<Option<FailureClass>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.as_deref().and_then(FailureClass::from_kebab))
}

/// Deserialize `contract_version` leniently by value *shape*:
/// absent/null reads as `None` (a schema-1 producer predating the handshake),
/// a string reads as itself, and any other JSON shape reads as the empty
/// string — a present-but-uninterpretable echo that can never equal
/// [`CONTRACT_VERSION`], so it lands on contract drift rather than failing
/// the whole document parse. That mirrors `failure_class` leniency: a
/// producer fumbling one field must not cost the document its infra signals.
fn deserialize_lenient_contract_version<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => Ok(Some(String::new())),
    }
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
/// Parses every schema_version in [`SUPPORTED_SCHEMA_VERSIONS`] — 1 (the
/// pre-taxonomy producer) and 2 (the failure-taxonomy contract; same shape,
/// the class is derived for every instrumented failing suite, not just
/// gates). Later versions may add fields; a schema_version this parser does
/// not know is a loud parse error (and thus an exit-code-only degradation),
/// never a guess.
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

    /// The `contract_version` echo (plan §"Versioning & compatibility",
    /// "Remote contract"): the template echoes back verbatim the version the
    /// client sent as the Workflow's `contract-version` parameter. Absent
    /// means a schema-1 producer predating the handshake (additive evolution:
    /// not drift); present and different from [`CONTRACT_VERSION`] — or
    /// present in a shape this client cannot read — is contract drift, and
    /// [`VerdictJson::to_verdict`] classifies the document as
    /// [`Verdict::InfraFailure`] no matter what its other fields claim.
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_contract_version",
        skip_serializing_if = "Option::is_none"
    )]
    pub contract_version: Option<String>,
}

impl VerdictJson {
    /// Parse verdict.json from a JSON string.
    ///
    /// Returns Err if JSON is malformed or schema_version is unsupported
    /// (outside [`SUPPORTED_SCHEMA_VERSIONS`]).
    pub fn parse(json: &str) -> Result<Self, BackendError> {
        let parsed: Self = serde_json::from_str(json)
            .map_err(|e| BackendError::new(&format!("failed to parse verdict.json: {}", e)))?;

        // Validate schema version
        if !SUPPORTED_SCHEMA_VERSIONS.contains(&parsed.schema_version) {
            return Err(BackendError::new(&format!(
                "unsupported verdict.json schema version: {}",
                parsed.schema_version
            )));
        }

        Ok(parsed)
    }

    /// Exit-code-only classification for a run whose verdict.json is missing
    /// or unusable: the workflow phase plus the best-known exit code, through
    /// the same ladder a parsed document goes through.
    ///
    /// This is the single degradation path for all three shapes of "no usable
    /// verdict.json" — the `verdict` output parameter absent entirely, its
    /// JSON malformed, or its `schema_version` newer than [`VerdictJson::parse`]
    /// knows. The latter two surface as a typed [`BackendError`] from parse —
    /// never a panic — and the caller degrades here. The degradation knows
    /// only what a missing document can still vouch for, so it classifies as
    /// if (phase, exit_code) were the whole document: a workflow that itself
    /// errored is [`Verdict::InfraFailure`] no matter what an exit code claims
    /// (no suite ever ran), and otherwise the plain exit-code ladder decides.
    /// No oom/deadline signal and no gate attribution is assumed, because
    /// none is known.
    pub fn from_exit_code(phase: &str, exit_code: i32) -> Verdict {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            phase: phase.to_string(),
            exit_code,
            oom: false,
            deadline_exceeded: false,
            failure_class: None,
            // The client fabricated this document from the terminal phase —
            // there is no remote echo to check, and none is needed: the
            // ladder here runs on knowledge the client produced itself.
            contract_version: None,
        }
        .to_verdict()
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

    /// The contract-drift signal, shared by the verdict ladder and the
    /// client's message: `Some(echo)` when the document echoes a
    /// `contract_version` this client does not speak, `None` when the
    /// handshake confirms (echo matches) or does not apply (absent — a
    /// schema-1 producer predating the handshake; see [`CONTRACT_VERSION`]).
    pub fn contract_drift(&self) -> Option<&str> {
        self.contract_version
            .as_deref()
            .filter(|echo| *echo != CONTRACT_VERSION)
    }

    /// Convert the verdict.json to a Verdict using full ladder semantics.
    ///
    /// Precedence: contract drift first — a document that echoes a contract
    /// this client does not speak cannot vouch for anything its other fields
    /// claim, so it classifies as InfraFailure ("contract drift", plan
    /// §"Versioning & compatibility"), never a misread verdict — then
    /// infrastructure signals (OOMKilled, deadline exceeded, the workflow
    /// itself erroring), then explicit gate attribution, then the exit-code
    /// ladder. An absent verdict.json never reaches this method; it degrades
    /// to exit-code-only via [`Verdict::interpret`].
    pub fn to_verdict(&self) -> Verdict {
        // Contract drift outranks everything: the document's own meaning is
        // what is in question.
        if self.contract_drift().is_some() {
            return Verdict::InfraFailure;
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    // --- verdict.json parsing: failure classes ------------------------------

    /// Build a schema-1 verdict.json document with the given fields
    /// (`failure_class` omitted when None). Always carries the matching
    /// `contract_version` echo: a post-handshake producer confirms the
    /// contract, and the handshake tests below build their mismatching /
    /// absent / garbled variants on top of this helper.
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
            "contract_version": CONTRACT_VERSION,
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

    /// FailureClass serde roundtrip: the kebab-case wire names survive
    /// serialize -> deserialize as the identity for every variant.
    #[test]
    fn failure_class_serde_round_trip_is_identity() {
        for class in [
            FailureClass::CompileError,
            FailureClass::TestFailure,
            FailureClass::Doctest,
            FailureClass::HarnessPanic,
            FailureClass::GateFailure,
        ] {
            let serialized = serde_json::to_string(&class).expect("serialize");
            let round: FailureClass =
                serde_json::from_str(&serialized).unwrap_or_else(|e| panic!("{serialized}: {e}"));
            assert_eq!(round, class, "round trip broke {serialized}");
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

    /// A `failure_class` whose value is not a JSON string is a malformed
    /// document, not an unknown class: serde rejects it outright with the
    /// document-level parse error, and the caller degrades to exit-code-only.
    /// Leniency is reserved for class *strings* this build does not know —
    /// see [`unknown_failure_class_degrades_to_absent_not_error`] — and for
    /// null, which is the documented shape of "absent".
    #[test]
    fn invalid_failure_class_value_is_rejected_by_serde() {
        let bad_values = [
            "42",
            "1.5",
            "true",
            "false",
            "[]",
            r#"{"class": "test-failure"}"#,
        ];
        for bad in bad_values {
            let doc = format!(
                r#"{{"schema_version": 1, "phase": "Failed", "exit_code": 1,
                    "failure_class": {bad}}}"#
            );
            let err = VerdictJson::parse(&doc)
                .expect_err(&format!("failure_class {bad} must be rejected by serde"));
            assert!(
                err.reason.starts_with("failed to parse verdict.json"),
                "failure_class {bad}: wrong error: {}",
                err.reason
            );
        }
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

    /// A complete schema-1 payload parses field-for-field: every documented
    /// field lands in the struct with the value the producer wrote.
    #[test]
    fn parse_accepts_complete_schema_one_payload() {
        let doc = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "test-failure"
        }"#;
        let vj = VerdictJson::parse(doc).expect("complete document must parse");
        assert_eq!(vj.schema_version, 1);
        assert_eq!(vj.phase, "Failed");
        assert_eq!(vj.exit_code, 1);
        assert!(!vj.oom);
        assert!(!vj.deadline_exceeded);
        assert_eq!(vj.failure_class, Some(FailureClass::TestFailure));
        assert_eq!(vj.to_verdict(), Verdict::TestFailure);
    }

    /// Malformed JSON is Err (naming the document), not a panic or a guess —
    /// the exit-code degradation happens in the caller (`Verdict::interpret`).
    #[test]
    fn parse_rejects_malformed_json() {
        let broken = [
            "{not json",
            "",
            "[]",
            "null",
            r#"{"schema_version": 1,}"#,
            r#"{"schema_version": 1}"#, // phase and exit_code are required
        ];
        for doc in broken {
            let err = VerdictJson::parse(doc).expect_err(&format!("{doc:?} must be rejected"));
            assert!(
                err.reason.starts_with("failed to parse verdict.json"),
                "{doc:?}: wrong error: {}",
                err.reason
            );
        }
    }

    /// Schema version 2 — the failure-taxonomy contract (verdict.json v2,
    /// plan §Component 5) — parses with the exact same shape and ladder as
    /// version 1: the bump is a *semantics* contract (a v2 producer derives
    /// the failure class for every instrumented failing suite, not just
    /// gates), which is precisely what the version field exists to carry. A
    /// v2 document therefore needs no new fields to be fully interpretable.
    #[test]
    fn schema_version_two_the_taxonomy_contract_parses_like_one() {
        let v2 = r#"{
            "schema_version": 2,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "compile-error",
            "contract_version": "1"
        }"#;
        let vj = VerdictJson::parse(v2).expect("schema_version 2 must parse");
        assert_eq!(vj.schema_version, 2);
        assert_eq!(vj.failure_class, Some(FailureClass::CompileError));
        assert_eq!(vj.to_verdict(), Verdict::TestFailure);
    }

    /// The two schema versions outside [`SUPPORTED_SCHEMA_VERSIONS`] that a
    /// producer is most likely to reach for — 0 (pre-versioning) and 3 (the
    /// first future version) — are rejected with an error that names the
    /// offending version, so a producer/consumer version mismatch is
    /// diagnosable from the message alone and callers degrade to
    /// exit-code-only rather than guessing.
    #[test]
    fn schema_version_zero_and_three_are_rejected_with_a_clear_error() {
        for version in [0u32, 3] {
            let doc =
                format!(r#"{{"schema_version": {version}, "phase": "Succeeded", "exit_code": 0}}"#);
            let err = VerdictJson::parse(&doc)
                .expect_err(&format!("schema_version {version} must be rejected"));
            assert!(
                err.reason.contains("schema version"),
                "{version}: wrong error: {}",
                err.reason
            );
            assert!(
                err.reason.contains(&version.to_string()),
                "{version}: error must name the version: {}",
                err.reason
            );
        }
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

    // --- failure taxonomy: FailureClass::classify (verdict.json v2) ---------

    // The fixtures live in one shared corpus
    // (tests/fixtures/failure-class-corpus.json) that both consumers of the
    // taxonomy read: the classify tests here, and the producer-parity test
    // below, which executes the reference producer's own classification
    // snippet (contrib/argo/gantry-verify-workflowtemplate.yml) on every
    // entry. Both sides of the pin — this classifier and the producer's
    // jq/grep/awk — must classify identically or agents branch on a class
    // the run contradicts.

    /// One fixture of the shared producer-reference corpus: exactly the
    /// inputs the producer archives (exit code, instrumentation, the two
    /// streams) plus the class both implementations must stamp.
    #[derive(Debug, Deserialize)]
    struct ProducerCorpusEntry {
        name: String,
        /// What the fixture proves, rendered on failure.
        why: String,
        /// The raw cargo exit code (0-255 shell domain).
        exit_code: i32,
        /// False when the caller chose their own `--message-format`.
        instrumented: bool,
        /// The archived cargo JSON protocol stream (one object per line,
        /// mixed with the harness's human lines that share stdout).
        messages: String,
        /// The human-visible run output (harness lines, rendered
        /// diagnostics, stderr).
        run_log: String,
        /// The kebab-case class both implementations must stamp, null where
        /// the classifiable window leaves the class unset.
        expected_class: Option<String>,
        /// Set only where the producer attributes the class explicitly
        /// (gates) instead of deriving it from the streams.
        producer_attribute: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct ProducerCorpus {
        entries: Vec<ProducerCorpusEntry>,
    }

    /// The shared corpus, exactly as both consumers read it.
    fn producer_corpus() -> ProducerCorpus {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/failure-class-corpus.json"
        )))
        .expect("the shared producer-reference corpus must parse")
    }

    /// The corpus is the fixture pin on [`FailureClass::classify`]: for
    /// every entry, classify must return exactly the entry's expected class
    /// for exactly the inputs the producer archives.
    #[test]
    fn classify_matches_the_producer_reference_corpus() {
        for entry in producer_corpus().entries {
            let name = entry.name.as_str();
            let why = entry.why.as_str();
            let expected = entry.expected_class.as_deref().map(|raw| {
                FailureClass::from_kebab(raw)
                    .unwrap_or_else(|| panic!("corpus entry {name} names an unknown class {raw:?}"))
            });
            assert_eq!(
                FailureClass::classify(
                    entry.exit_code,
                    entry.instrumented,
                    &entry.messages,
                    &entry.run_log
                ),
                expected,
                "entry {name}: {why}"
            );
        }
    }

    /// The window's upper edge across the whole signal range: the corpus
    /// carries the representative edges, this sweeps the ladder — every
    /// signal-killed exit is infra, classified by the verdict ladder, never
    /// a failure class.
    #[test]
    fn classify_signal_range_is_classless_across_the_ladder() {
        let corpus = producer_corpus();
        let entry = corpus
            .entries
            .iter()
            .find(|entry| entry.name == "signal-killed-suite-is-infra")
            .expect("the corpus carries the signal-range fixture");
        for code in [128, 130, 137, 143, 255] {
            assert_eq!(
                FailureClass::classify(code, entry.instrumented, &entry.messages, &entry.run_log),
                None,
                "exit {code} is infra, not a failure class"
            );
        }
    }

    /// `GateFailure` is never derived from the streams — not on any corpus
    /// fixture. Gates run only after a passing suite and are attributed
    /// explicitly by the producer, so exactly one fixture carries the
    /// attribution marker, and on it the classifier must derive nothing.
    #[test]
    fn classify_never_derives_gate_failure() {
        let corpus = producer_corpus();
        for entry in &corpus.entries {
            let name = entry.name.as_str();
            assert_ne!(
                FailureClass::classify(
                    entry.exit_code,
                    entry.instrumented,
                    &entry.messages,
                    &entry.run_log
                ),
                Some(FailureClass::GateFailure),
                "entry {name}: gate-failure is attributed, never derived"
            );
        }
        let attributed: Vec<&ProducerCorpusEntry> = corpus
            .entries
            .iter()
            .filter(|entry| entry.producer_attribute.is_some())
            .collect();
        let gate = attributed
            .first()
            .expect("the corpus carries the producer-attributed gate fixture");
        assert_eq!(
            attributed.len(),
            1,
            "gate-failure is the only attributed class"
        );
        assert_eq!(
            gate.producer_attribute.as_deref(),
            Some("gate-failure"),
            "gate-failure is the only attributed class"
        );
        assert_eq!(
            gate.exit_code, 0,
            "gates run only after a passing suite: the canonical gate shape"
        );
        assert_eq!(
            gate.expected_class, None,
            "attribution is not derivation: the window leaves the class unset"
        );
    }

    // --- producer parity: the template's snippet vs classify -----------------

    /// Extract the producer's classification snippet verbatim from the
    /// reference template: the `failure_class` assignment chain between the
    /// failure-taxonomy and quality-gates section markers — the exact text
    /// the producer executes when it stamps `failure_class` into
    /// verdict.json.
    ///
    /// Extraction is structural on purpose: what runs is what the template
    /// says, so a template reorganization that moves or breaks the block
    /// fails here, loudly, rather than quietly unpinning the producer.
    fn classification_snippet(template: &str) -> Result<String, String> {
        const TAXONOMY: &str = "# --- failure taxonomy";
        const GATES: &str = "# --- quality gates";
        let lines: Vec<&str> = template.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains(TAXONOMY))
            .ok_or_else(|| format!("template lost its {TAXONOMY:?} section marker"))?;
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.contains(GATES))
            .map(|offset| start + 1 + offset)
            .ok_or_else(|| format!("template lost its {GATES:?} section marker"))?;
        let region = &lines[start..end];
        let begin = region
            .iter()
            .position(|line| line.trim() == r#"failure_class="""#)
            .ok_or_else(|| "classification block lost its failure_class initializer".to_string())?;
        let mut snippet: Vec<&str> = region[begin..].to_vec();
        while snippet.last().is_some_and(|line| line.trim().is_empty()) {
            snippet.pop();
        }
        let last = snippet
            .last()
            .ok_or_else(|| "classification block is empty".to_string())?;
        if last.trim() != "fi" {
            return Err(format!(
                "classification block must end at the window gate's closing `fi`, found {last:?}"
            ));
        }
        Ok(snippet.join("\n"))
    }

    /// Stamp `failure_class` the way the producer does: run the template's
    /// classification snippet under the shell options the template runs
    /// under, with the fixture's streams archived to files exactly as the
    /// producer archives them. Returns the snippet's class ("" where the
    /// window or instrumentation gates leave it unset).
    ///
    /// The snippet's tools are the producer's own (jq, grep, awk); an
    /// environment that cannot run them cannot establish parity, so this
    /// fails loudly rather than skipping the pin.
    fn run_producer_snippet(
        entry: &ProducerCorpusEntry,
        snippet: &str,
        work: &tempfile::TempDir,
    ) -> String {
        let messages = work.path().join("cargo-messages.jsonl");
        std::fs::write(&messages, &entry.messages).expect("archive the protocol stream");
        let run_log = work.path().join("output.log");
        std::fs::write(&run_log, &entry.run_log).expect("archive the run log");
        // The template's fmt_present gate is the negation of classify's
        // `instrumented` input: a caller-chosen --message-format means the
        // raw protocol was never captured.
        let fmt_present = if entry.instrumented { "false" } else { "true" };
        let exit_code = entry.exit_code;
        let messages = messages.display().to_string();
        let run_log = run_log.display().to_string();
        let script = format!(
            "set -euo pipefail\nexit_code={exit_code}\nfmt_present={fmt_present}\n\
             messages={messages}\nrun_log={run_log}\n{snippet}\nprintf '%s' \"$failure_class\"\n"
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("spawn bash to run the producer's classification snippet");
        let name = entry.name.as_str();
        assert!(
            output.status.success(),
            "entry {name}: the producer snippet must run cleanly, stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("the stamped class is UTF-8")
    }

    /// The pin: the reference producer's snippet and
    /// [`FailureClass::classify`] must stamp the same class on every fixture
    /// of the shared corpus. Drift anywhere — the window gate, the jq
    /// compile scan, the grep/awk chain, a class name — fails on the
    /// fixture that diverged, before an agent ever branches on a class the
    /// run contradicts.
    #[test]
    fn producer_snippet_and_classify_agree_on_the_shared_corpus() {
        let template = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/contrib/argo/gantry-verify-workflowtemplate.yml"
        ));
        let snippet = classification_snippet(template)
            .expect("the template carries the classification block");
        let corpus = producer_corpus();
        let work = tempfile::TempDir::new().expect("scratch dir for the archived streams");
        for entry in &corpus.entries {
            let name = entry.name.as_str();
            let why = entry.why.as_str();
            let stamped = run_producer_snippet(entry, &snippet, &work);
            let derived = FailureClass::classify(
                entry.exit_code,
                entry.instrumented,
                &entry.messages,
                &entry.run_log,
            );
            let derived_kebab = derived.as_ref().map(|class| {
                serde_json::to_value(class)
                    .expect("a failure class serializes")
                    .as_str()
                    .expect("the serialized class is its kebab name")
                    .to_owned()
            });

            // 1. Parity: the producer stamps exactly what classify derives.
            assert_eq!(
                stamped,
                derived_kebab.clone().unwrap_or_default(),
                "entry {name}: template stamped {stamped:?}, classify derived \
                 {derived_kebab:?}: {why}"
            );

            // 2. The corpus is load-bearing for both consumers: the class
            //    both implementations stamped is also the entry's expectation.
            let stamped_class = if stamped.is_empty() {
                None
            } else {
                Some(stamped.as_str())
            };
            assert_eq!(
                stamped_class,
                entry.expected_class.as_deref(),
                "entry {name}: the corpus's expected_class must name what both \
                 implementations stamp"
            );

            // 3. The taxonomy never derives gate-failure on either side;
            //    where the corpus marks an attributed class, the template
            //    must carry the explicit attribution, at the suite's own
            //    exit 0.
            assert_ne!(
                stamped, "gate-failure",
                "entry {name}: the classification window must never derive gate-failure"
            );
            if entry.producer_attribute.as_deref() == Some("gate-failure") {
                let attribution = template
                    .lines()
                    .find(|line| line.contains("emit_verdict") && line.contains("gate-failure"))
                    .expect("the template must attribute gate-failure explicitly");
                let args: Vec<&str> = attribution.split_whitespace().collect();
                assert_eq!(
                    args.len(),
                    5,
                    "emit_verdict takes phase exit_code class oom"
                );
                assert_eq!(args[1], "Failed", "a failed gate is a Failed verdict");
                assert_eq!(
                    args[2], "0",
                    "the suite passed: gate attribution keeps its exit 0 ({attribution:?})"
                );
                assert_eq!(
                    args[3], "gate-failure",
                    "the attributed class is stamped verbatim ({attribution:?})"
                );
            }
        }
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

    /// The other canonical gate shape (moved here from the argo backend's old
    /// parser tests): the suite passed but the gate failed, so the remote
    /// wraps the run as overall exit 1 with explicit gate attribution. The
    /// attribution must survive that failing exit code — a GateFailure is a
    /// test result (never the local-fallback InfraFailure) and it reports the
    /// exit code a test failure would.
    #[test]
    fn gate_failure_attribution_survives_a_failing_exit_code() {
        let vj = VerdictJson::parse(&verdict_doc(
            "Failed",
            1,
            false,
            false,
            Some("gate-failure"),
        ))
        .expect("gate document must parse");
        let verdict = vj.to_verdict();
        assert_eq!(verdict, Verdict::GateFailure);
        assert!(!verdict.is_infra_failure());
        assert!(verdict.has_test_result());
        assert_eq!(verdict.to_exit_code(), 1);
    }

    /// The exit-code ladder inside to_verdict: with no infra signals and no
    /// gate attribution, the exit code alone decides — including the >=2
    /// bucket, which stays InfraFailure with an otherwise-clean document.
    #[test]
    fn to_verdict_exit_code_ladder_without_infra_signals() {
        let pass = VerdictJson::parse(&verdict_doc("Succeeded", 0, false, false, None))
            .expect("pass document must parse");
        assert_eq!(pass.to_verdict(), Verdict::Pass);

        let failed = VerdictJson::parse(&verdict_doc("Failed", 1, false, false, None))
            .expect("test-failure document must parse");
        assert_eq!(failed.to_verdict(), Verdict::TestFailure);

        for code in [2, 3, 101, 137, 255, -1] {
            let vj = VerdictJson::parse(&verdict_doc("Failed", code, false, false, None))
                .unwrap_or_else(|e| panic!("exit {code} must parse: {e}"));
            assert_eq!(vj.to_verdict(), Verdict::InfraFailure, "exit {code}");
        }
    }

    /// Each infra signal on its own outranks a passing exit code: oom and
    /// deadline_exceeded must each turn an exit-0 document into InfraFailure.
    #[test]
    fn each_infra_signal_outranks_a_passing_exit_code() {
        let oom = VerdictJson::parse(&verdict_doc("Failed", 0, true, false, None))
            .expect("oom document must parse");
        assert_eq!(oom.to_verdict(), Verdict::InfraFailure);

        let deadline = VerdictJson::parse(&verdict_doc("Succeeded", 0, false, true, None))
            .expect("deadline document must parse");
        assert_eq!(deadline.to_verdict(), Verdict::InfraFailure);
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
            "schema_version": 3,
            "phase": "Succeeded",
            "exit_code": 0,
            "ladder": "v3"
        }"#;
        assert_eq!(Verdict::interpret(0, Some(future)), Verdict::Pass);
        assert_eq!(Verdict::interpret(1, Some(future)), Verdict::TestFailure);
    }

    // --- the exit-code-only classifier (VerdictJson::from_exit_code) ---------

    /// The classifier IS the document ladder: it must return exactly what a
    /// minimal schema-1 document carrying only (phase, exit_code) returns
    /// from to_verdict — the structural pin that the fallback and the parse
    /// path share one implementation.
    #[test]
    fn from_exit_code_is_the_minimal_document_ladder() {
        for phase in ["Succeeded", "Failed", "Error", "Pending", "Running", ""] {
            for code in [0, 1, 2, 137, -1] {
                let doc = VerdictJson {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    phase: phase.to_string(),
                    exit_code: code,
                    oom: false,
                    deadline_exceeded: false,
                    failure_class: None,
                    contract_version: None,
                };
                assert_eq!(
                    VerdictJson::from_exit_code(phase, code),
                    doc.to_verdict(),
                    "phase {phase:?}, exit {code}"
                );
            }
        }
    }

    /// A workflow that itself errored is InfraFailure regardless of the exit
    /// code — no suite ever ran, so an exit code claims nothing.
    #[test]
    fn from_exit_code_error_phase_outranks_the_exit_code() {
        for code in [0, 1, 2, 137, -1] {
            let verdict = VerdictJson::from_exit_code("Error", code);
            assert_eq!(verdict, Verdict::InfraFailure, "Error phase, exit {code}");
            assert!(verdict.is_infra_failure());
        }
    }

    /// Away from the Error rung the classifier is exactly the phase-less
    /// exit-code ladder — the two entry points must not drift.
    #[test]
    fn from_exit_code_matches_the_phase_less_ladder_off_the_error_rung() {
        for phase in ["Succeeded", "Failed", "Pending", "Running", ""] {
            for code in -500..=500 {
                assert_eq!(
                    VerdictJson::from_exit_code(phase, code),
                    Verdict::from_exit_code(code),
                    "phase {phase:?}, exit {code}"
                );
            }
        }
    }

    /// The Error rung is the one place the phase-aware classifier may disagree
    /// with [`Verdict::interpret`]'s phase-less degradation: interpret has no
    /// phase to consult, so an Error-phase run with a passing exit code reads
    /// Pass there, while this classifier reads InfraFailure — no suite ever
    /// ran, so the exit code claims nothing. Pinning the divergence keeps it
    /// a decision rather than an accident.
    #[test]
    fn from_exit_code_error_rung_is_the_one_divergence_from_interpret() {
        for code in [0, 1, 2, 137, -1] {
            assert_eq!(
                Verdict::interpret(code, None),
                Verdict::from_exit_code(code),
                "interpret is phase-less: exit {code}"
            );
            assert_eq!(
                VerdictJson::from_exit_code("Error", code),
                Verdict::InfraFailure,
                "the classifier knows the phase: exit {code}"
            );
        }
    }

    /// The pinned pairings the Argo fallback feeds it: Succeeded ⇒ exit 0 ⇒
    /// Pass, Failed ⇒ exit 1 ⇒ TestFailure, and the rungs with no test
    /// outcome (exit code unknown, fed as the ≥2 infra bucket) ⇒
    /// InfraFailure.
    #[test]
    fn from_exit_code_classifies_the_argo_fallback_pairings() {
        assert_eq!(VerdictJson::from_exit_code("Succeeded", 0), Verdict::Pass);
        assert_eq!(
            VerdictJson::from_exit_code("Failed", 1),
            Verdict::TestFailure
        );
        for phase in ["Pending", "Running", "Error"] {
            assert_eq!(
                VerdictJson::from_exit_code(phase, 2),
                Verdict::InfraFailure,
                "phase {phase:?}"
            );
        }
    }

    // --- the contract handshake (contract_version echo) ----------------------

    /// Swap the helper's matching echo for `echo_json` (a raw JSON value
    /// source). Matched against the compact form serde_json actually emits
    /// (no space after the colon).
    fn with_echo(doc: &str, echo_json: &str) -> String {
        doc.replace(
            &format!(r#""contract_version":"{}""#, CONTRACT_VERSION),
            &format!(r#""contract_version":{echo_json}"#),
        )
    }

    /// The drift signal fires exactly when a present echo differs from
    /// [`CONTRACT_VERSION`] — a matching echo and an absent echo are both
    /// "no drift" (the handshake confirmed, or a schema-1 producer predates
    /// it; see [`CONTRACT_VERSION`] for why absence is tolerance, not drift).
    #[test]
    fn contract_drift_signals_exactly_the_mismatching_echo() {
        let matching = VerdictJson::parse(&verdict_doc("Succeeded", 0, false, false, None))
            .expect("matching echo must parse");
        assert_eq!(matching.contract_drift(), None);
        assert_eq!(matching.contract_version.as_deref(), Some(CONTRACT_VERSION));

        let echo_less =
            VerdictJson::parse(r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0}"#)
                .expect("echo-less document must parse");
        assert_eq!(echo_less.contract_version, None);
        assert_eq!(echo_less.contract_drift(), None);

        for stray in ["\"2\"", "\"0\"", "\"v9\"", "\"\"", "\"1 \""] {
            let doc = with_echo(&verdict_doc("Succeeded", 0, false, false, None), stray);
            let vj = VerdictJson::parse(&doc)
                .unwrap_or_else(|e| panic!("stray echo {stray} must parse: {e}"));
            let expected = stray.trim_matches('"');
            assert_eq!(vj.contract_drift(), Some(expected), "echo {stray}");
        }
    }

    /// The plan's handshake rule (§"Versioning & compatibility"): a mismatch
    /// the client can't interpret is InfraFailure with drift semantics,
    /// never a misread verdict. Across the full field matrix, swapping the
    /// matching echo for a foreign one flips every document — pass, gate,
    /// OOM, deadline, whatever — to InfraFailure.
    #[test]
    fn property_mismatching_echo_is_infra_failure_across_the_matrix() {
        for base in all_schema_one_documents() {
            let expected = VerdictJson::parse(&base).expect("base parses").to_verdict();
            // Sanity: with the matching echo the matrix classifies normally
            // (this is what makes the flip below a drift effect, not noise).
            assert_ne!(expected, Verdict::Cancelled, "matrix verdict is real");

            let drifted = with_echo(&base, r#""9""#);
            let parsed = VerdictJson::parse(&drifted)
                .unwrap_or_else(|e| panic!("{drifted}: drift broke parse: {e}"));
            assert_eq!(parsed.contract_drift(), Some("9"));
            assert_eq!(
                parsed.to_verdict(),
                Verdict::InfraFailure,
                "foreign echo must read as infra: {drifted}"
            );
            assert!(parsed.to_verdict().is_infra_failure());
        }
    }

    /// A producer that echoes a non-string shape tried to confirm a contract
    /// this client cannot read: the document still parses (one fumbled field
    /// must not cost the rest), but the unreadable echo can never match, so
    /// it is drift — InfraFailure, not a guessed verdict.
    #[test]
    fn garbled_echo_shape_is_drift_not_parse_error() {
        for garbled in ["42", "true", "[\"1\"]", "{\"v\":1}"] {
            let doc = with_echo(&verdict_doc("Succeeded", 0, false, false, None), garbled);
            let parsed = VerdictJson::parse(&doc)
                .unwrap_or_else(|e| panic!("garbled echo {garbled} must still parse: {e}"));
            assert!(parsed.contract_drift().is_some(), "garbled {garbled}");
            assert_eq!(
                parsed.to_verdict(),
                Verdict::InfraFailure,
                "garbled echo {garbled}"
            );
        }
    }

    /// Absence of the echo is tolerance, not drift — the field arrived by
    /// additive evolution, so a schema-1 producer predating the handshake is
    /// fully interpretable and the ladder decides exactly as before.
    #[test]
    fn absent_echo_is_a_pre_handshake_producer_not_drift() {
        let pass =
            VerdictJson::parse(r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0}"#)
                .expect("echo-less pass must parse");
        assert_eq!(pass.contract_drift(), None);
        assert_eq!(pass.to_verdict(), Verdict::Pass);

        let gate = VerdictJson::parse(
            r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0,
                "failure_class": "gate-failure"}"#,
        )
        .expect("echo-less gate document must parse");
        assert_eq!(gate.contract_drift(), None);
        assert_eq!(gate.to_verdict(), Verdict::GateFailure);

        let oom = VerdictJson::parse(
            r#"{"schema_version": 1, "phase": "Failed", "exit_code": 137,
                "oom": true, "deadline_exceeded": false}"#,
        )
        .expect("echo-less oom document must parse");
        assert_eq!(oom.to_verdict(), Verdict::InfraFailure);
    }

    /// The handshake does not disturb the degradation contract: malformed
    /// JSON and unsupported schema versions still degrade to exit-code-only
    /// regardless of any echo they carry — there is no parseable schema-1
    /// document to handshake with.
    #[test]
    fn handshake_does_not_disturb_the_degradation_contract() {
        assert_eq!(
            Verdict::interpret(1, Some(r#"{not json, "contract_version": "9"}"#)),
            Verdict::TestFailure,
            "unparseable document degrades on the exit code alone"
        );
        assert_eq!(
            Verdict::interpret(
                0,
                Some(
                    r#"{"schema_version": 3, "phase": "Succeeded",
                "exit_code": 0, "contract_version": "9"}"#
                )
            ),
            Verdict::Pass,
            "unsupported schema degrades; the echo is never consulted"
        );
    }

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
        // contract_version is NOT here: it is a known field since the
        // contract handshake, and injecting into it is drift semantics —
        // covered by property_mismatching_echo_is_infra_failure_across_the_matrix
        // above, not by the unknown-field tolerance.
        const NAMES: &[&str] = &["future_field", "toolchain", "node_name"];
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
            if parsed.contract_version.is_none() {
                assert!(
                    !serialized.contains("contract_version"),
                    "absent contract_version must not serialize: {serialized}"
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
    /// interpreted as a known one.
    #[test]
    fn property_unsupported_schema_versions_are_rejected() {
        for version in [0u32, 3, 42, u32::MAX] {
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
