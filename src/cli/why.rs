// gantry — `gantry why` (plan §"CLI surface"): replay the last run's
// gate/decision trace from the write-ahead ledger.
//
// The plan's example answer is literally "clean tree: NO ← here" — the gate
// trace pointing at the check that flipped the decision. The machine answer
// (`--json`) is the primary interface (same section: "the JSON surface is
// the primary interface, human text is a rendering of it"): a NEEDLE worker
// that hit a degradation branches on `verdict.verdict` — the AS-2 failure
// class — without parsing logs or stderr.
//
// Exit codes: 0 a last run was found, 1 the ledger is empty or unreadable
// (nothing to explain), 2 usage error.

use super::{
    decision_name, failure_class_name, gate_trace_line, iso8601_utc, ran_name, RunJson,
    SCHEMA_VERSION,
};
use crate::runlog::{Ledger, RunLog};
use serde::{Deserialize, Serialize};

/// The `gantry why --json` document. Published schema: `docs/schemas/
/// why-v1.json`, embedded as [`super::WHY_SCHEMA`] and validated against by
/// the tests below — the published file is load-bearing, not decorative.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WhyDoc {
    /// Contract version ([`SCHEMA_VERSION`]); the schema pins it with
    /// `{"const": 1}`.
    pub schema_version: u32,
    /// True when the ledger holds at least one run; false (with a null run)
    /// on an empty or missing ledger.
    pub found: bool,
    /// The most recent run, `None` when [`WhyDoc::found`] is false.
    pub run: Option<RunJson>,
    /// Health of the ledger the answer was read from.
    pub ledger: LedgerHealth,
    /// Absolute path of the ledger the answer was read from.
    pub runlog_path: String,
}

/// Ledger health alongside the answer: a damaged ledger must not brick the
/// diagnostic that exists to explain damaged runs ([`RunLog::read_entries`]
/// degrades instead of erroring), but the number of records it had to skip
/// stays visible so a torn ledger is never mistaken for a quiet one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerHealth {
    /// Paired runs read from the ledger.
    pub entries: usize,
    /// Unparseable lines skipped (torn final line, truncated record).
    pub skipped_lines: usize,
    /// Verdict records whose `run_id` matched no intent.
    pub unmatched_verdicts: usize,
}

impl LedgerHealth {
    fn from_ledger(ledger: &Ledger) -> Self {
        LedgerHealth {
            entries: ledger.entries.len(),
            skipped_lines: ledger.skipped_lines,
            unmatched_verdicts: ledger.unmatched_verdicts,
        }
    }
}

/// Build the document from a ledger already read. Deliberately pure — tests
/// fabricate ledgers instead of touching the process state dir (hermeticity
/// rule); `cli` below is the only thing that does I/O.
pub fn build(ledger: Ledger, runlog_path: String) -> WhyDoc {
    // "Last run" = the newest entry in file order. runs.jsonl is append-only
    // with O_APPEND single-line writes, so file order is arrival order.
    let found = ledger.entries.last().is_some();
    let run = ledger.entries.last().map(RunJson::from_entry);
    WhyDoc {
        schema_version: SCHEMA_VERSION,
        found,
        run,
        ledger: LedgerHealth::from_ledger(&ledger),
        runlog_path,
    }
}

/// Human rendering — explicitly a rendering of the JSON document, never a
/// second source of truth.
pub fn render_text(doc: &WhyDoc) -> String {
    if !doc.found {
        return format!(
            "no runs recorded in {} — nothing to explain yet\n",
            doc.runlog_path
        );
    }
    let run = doc.run.as_ref().expect("found implies a run");

    let mut lines = Vec::with_capacity(6);
    lines.push(format!("run {} · {}", run.run_id, iso8601_utc(run.ts)));
    lines.push(format!(
        "{} {} — {} @ {}",
        run.tool,
        run.args.join(" "),
        run.repo,
        run.sha
    ));
    lines.push(format!(
        "decision: {} · backend: {}",
        decision_name(run.decision),
        run.backend
    ));
    lines.push(gate_trace_line(&run.gate));
    if !run.reason.is_empty() {
        lines.push(format!("reason: {}", run.reason));
    }
    match &run.verdict {
        Some(verdict) => {
            // The taxonomy class, when the remote pipeline recorded one —
            // the thing an agent branches on without parsing logs. Absent
            // for every record that carries no class, so the line stays
            // exactly as long as it needs to be.
            let class = run
                .failure_class
                .as_ref()
                .map(|class| format!(" · class {}", failure_class_name(class)))
                .unwrap_or_default();
            lines.push(format!(
                "verdict: {} · ran {} · exit {} · handle {}{}",
                verdict.verdict,
                ran_name(verdict.ran),
                verdict.exit_code,
                verdict.handle,
                class
            ))
        }
        None => lines.push(
            "verdict: none recorded — in flight, or a lost run (gantry doctor reports those)"
                .to_string(),
        ),
    }
    lines.join("\n") + "\n"
}

/// Entry point from main.rs: `gantry why [--json]` (the leading `gantry why`
/// already stripped from `argv`).
pub fn cli(argv: &[String]) -> u8 {
    let mut json = false;
    for arg in argv {
        match arg.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("gantry why: unexpected argument '{other}'");
                eprintln!("usage: gantry why [--json]");
                return 2;
            }
        }
    }

    match read_doc() {
        Ok(doc) => {
            emit(&doc, json);
            // 1 on an empty ledger: a caller branching on the exit code gets
            // "there is no last run" without parsing the document.
            if doc.found {
                0
            } else {
                1
            }
        }
        Err(e) => {
            // EC-08 stance for diagnostics: an unreadable ledger never crashes
            // the command — say why on stderr and exit 1 (nothing to explain).
            eprintln!("gantry why: {e}");
            1
        }
    }
}

/// Read the real ledger and build the document. The only I/O in the module.
fn read_doc() -> Result<WhyDoc, String> {
    let runlog = RunLog::open().map_err(|e| format!("cannot open ledger: {e}"))?;
    let path = runlog.path().display().to_string();
    let ledger = runlog
        .read_entries()
        .map_err(|e| format!("cannot read ledger: {e}"))?;
    Ok(build(ledger, path))
}

/// Print the document in the requested surface.
fn emit(doc: &WhyDoc, json: bool) {
    if json {
        match serde_json::to_string_pretty(doc) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("gantry why: cannot serialize document: {e}"),
        }
    } else {
        print!("{}", render_text(doc));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{schema, WHY_SCHEMA};
    use crate::runlog::{
        Decision, GateInputs, IntentRecord, RanLocation, RunEntry, Verdict, VerdictRecord,
    };
    use std::path::PathBuf;

    const FAKE_LEDGER: &str = "/tmp/gantry-test/runs.jsonl";

    fn intent(clean: bool, decision: Decision) -> IntentRecord {
        IntentRecord::new(
            "cargo".to_string(),
            vec!["test".to_string()],
            "https://github.com/example/repo".to_string(),
            "abc123".to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean,
            },
            decision,
            if clean {
                String::new()
            } else {
                "dirty".to_string()
            },
            "command".to_string(),
        )
    }

    /// A ledger entry, optionally completed with a verdict. `exit_code`
    /// mirrors the ladder: 0 pass, non-zero anything else.
    fn entry(clean: bool, verdict: Option<Verdict>) -> RunEntry {
        let intent = intent(clean, Decision::Remote);
        let verdict = verdict.map(|verdict| {
            let exit_code = if verdict == Verdict::Pass { 0 } else { 1 };
            VerdictRecord::new(
                intent.run_id.clone(),
                verdict,
                RanLocation::Remote,
                exit_code,
                "gantry-x7k2p".to_string(),
                None,
            )
        });
        RunEntry { intent, verdict }
    }

    fn ledger(entries: Vec<RunEntry>) -> Ledger {
        Ledger {
            entries,
            skipped_lines: 0,
            unmatched_verdicts: 0,
        }
    }

    /// The same entry with the remote pipeline's taxonomy class stamped on
    /// its verdict record — the shape an instrumented failing remote run
    /// writes (verdict.json v2, plan §Component 5).
    fn classified(entry: RunEntry, class: crate::verdict::FailureClass) -> RunEntry {
        let mut verdict = entry.verdict;
        if let Some(record) = verdict.as_mut() {
            record.failure_class = Some(class);
        }
        RunEntry {
            intent: entry.intent,
            verdict,
        }
    }

    /// Validate a serializable document against its published schema.
    fn assert_valid(schema_text: &str, doc: &impl Serialize) {
        let schema: serde_json::Value =
            serde_json::from_str(schema_text).expect("published schema parses");
        let doc = serde_json::to_value(doc).expect("document serializes");
        schema::validate(&schema, &doc)
            .unwrap_or_else(|errors| panic!("document violates its published schema: {errors:?}"));
    }

    #[test]
    fn the_last_run_is_the_one_replayed() {
        let doc = build(
            ledger(vec![
                entry(true, Some(Verdict::Pass)),
                entry(true, None), // newer, still in flight
            ]),
            FAKE_LEDGER.to_string(),
        );

        assert!(doc.found);
        let run = doc.run.expect("found implies a run");
        assert!(run.orphaned, "the newest entry is the in-flight one");
        assert!(doc.ledger.entries == 2);
    }

    #[test]
    fn an_empty_ledger_is_found_false_with_a_null_run() {
        let doc = build(ledger(vec![]), FAKE_LEDGER.to_string());
        assert!(!doc.found);
        assert_eq!(doc.run, None);
    }

    #[test]
    fn why_json_validates_against_its_published_schema() {
        // The acceptance for bf-1n4, over the document shapes the command can
        // actually emit: a completed failing run (the AS-2 shape), the same
        // run with a taxonomy class on its verdict record, an in-flight
        // orphan, and an empty ledger.
        let completed = build(
            ledger(vec![entry(false, Some(Verdict::TestFailure))]),
            FAKE_LEDGER.to_string(),
        );
        let classified = build(
            ledger(vec![classified(
                entry(false, Some(Verdict::TestFailure)),
                crate::verdict::FailureClass::TestFailure,
            )]),
            FAKE_LEDGER.to_string(),
        );
        let orphan = build(ledger(vec![entry(true, None)]), FAKE_LEDGER.to_string());
        let empty = build(ledger(vec![]), FAKE_LEDGER.to_string());

        for doc in [&completed, &classified, &orphan, &empty] {
            assert_valid(WHY_SCHEMA, doc);
        }

        // And the JSON spellings are the ones the schema's enums describe.
        let doc = serde_json::to_value(&completed).unwrap();
        assert_eq!(doc["run"]["decision"], "remote");
        assert_eq!(doc["run"]["verdict"]["verdict"], "test_failure");
        assert_eq!(doc["run"]["verdict"]["ran"], "remote");
        assert_eq!(doc["run"]["orphaned"], false);
        // No class on the record reads as null, never an absent field.
        assert_eq!(doc["run"]["failure_class"], serde_json::Value::Null);
        let doc = serde_json::to_value(&classified).unwrap();
        assert_eq!(doc["run"]["failure_class"], "test-failure");
    }

    #[test]
    fn human_text_names_the_failure_class_of_a_failed_remote_run() {
        let doc = build(
            ledger(vec![classified(
                entry(false, Some(Verdict::TestFailure)),
                crate::verdict::FailureClass::TestFailure,
            )]),
            FAKE_LEDGER.to_string(),
        );
        let text = render_text(&doc);
        assert!(text.contains(" · class test-failure"), "{text}");
        assert!(text.contains("verdict: TestFailure"), "{text}");
    }

    #[test]
    fn human_text_omits_the_class_when_none_was_recorded() {
        // Passes, infra, cancels, local runs, uninstrumented producers: no
        // class on the record, no name in the line.
        let doc = build(
            ledger(vec![entry(false, Some(Verdict::TestFailure))]),
            FAKE_LEDGER.to_string(),
        );
        let text = render_text(&doc);
        assert!(!text.contains("class"), "{text}");
    }

    #[test]
    fn a_pre_taxonomy_record_reads_as_a_null_class_not_an_error() {
        // The pre-taxonomy producer shape: schema 1, no `failure_class` key
        // on disk. The ledger parse is lenient upstream; the projection must
        // read it as null and still explain the run.
        let legacy: VerdictRecord = serde_json::from_str(
            r#"{"rec":"verdict","schema_version":1,"run_id":"legacy","ts":2,"verdict":"test_failure","ran":"remote","exit_code":101,"handle":"gantry-x7k2p"}"#,
        )
        .expect("legacy record parses");
        assert!(legacy.failure_class.is_none());

        let doc = build(
            ledger(vec![RunEntry {
                intent: intent(true, Decision::Remote),
                verdict: Some(legacy),
            }]),
            FAKE_LEDGER.to_string(),
        );
        assert!(doc.found);
        let run = doc.run.as_ref().expect("found implies a run");
        assert!(run.failure_class.is_none());
        assert!(run.verdict.is_some(), "everything but the class survives");

        let text = render_text(&doc);
        assert!(!text.contains("class"), "{text}");
    }

    #[test]
    fn human_text_points_at_the_failing_check() {
        let doc = build(
            ledger(vec![entry(false, Some(Verdict::TestFailure))]),
            FAKE_LEDGER.to_string(),
        );
        let text = render_text(&doc);
        assert!(
            text.contains("clean tree NO ← here"),
            "the plan's replay example, verbatim: {text}"
        );
        assert!(text.contains("reason: dirty"), "{text}");
        assert!(text.contains("verdict: TestFailure"), "{text}");
    }

    #[test]
    fn human_text_names_lost_runs_on_a_missing_verdict() {
        let doc = build(ledger(vec![entry(true, None)]), FAKE_LEDGER.to_string());
        let text = render_text(&doc);
        assert!(text.contains("none recorded"), "{text}");
        assert!(text.contains("gantry doctor"), "{text}");
    }

    #[test]
    fn human_text_says_nothing_is_recorded_on_an_empty_ledger() {
        let doc = build(ledger(vec![]), FAKE_LEDGER.to_string());
        let text = render_text(&doc);
        assert!(text.contains("no runs recorded"), "{text}");
    }
}
