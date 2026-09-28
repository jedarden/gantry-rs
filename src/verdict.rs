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

#[cfg(test)]
mod tests {
    use super::*;

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
            let err =
                VerdictJson::parse(doc).expect_err(&format!("{doc:?} must be rejected"));
            assert!(
                err.reason.starts_with("failed to parse verdict.json"),
                "{doc:?}: wrong error: {}",
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
