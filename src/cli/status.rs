// gantry — `gantry status` (plan §"CLI surface"): recent and in-flight runs
// from the write-ahead ledger.
//
// The machine surface (`--json`) is primary: `in_flight` answers "is anything
// running right now (or was lost)" and `recent` answers "what did the last
// runs conclude" without a human reading stderr (plan §"CLI surface":
// diagnostics are agent-first).
//
// Exit codes: 0 once the arguments parse — an empty ledger is a valid status,
// and per EC-08 an unreadable one is reported on stderr with an empty answer,
// never a crash — and 2 on usage error.

use super::{decision_name, iso8601_utc, ran_name, RunJson, SCHEMA_VERSION};
use crate::runlog::{Ledger, RunLog};
use serde::{Deserialize, Serialize};

/// How many completed runs `recent` carries when `--limit` is not given.
pub const DEFAULT_LIMIT: usize = 10;

/// The `gantry status --json` document. Published schema: `docs/schemas/
/// status-v1.json`, embedded as [`super::STATUS_SCHEMA`] and validated
/// against by the tests below.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusDoc {
    /// Contract version ([`SCHEMA_VERSION`]); the schema pins it with
    /// `{"const": 1}`.
    pub schema_version: u32,
    /// Absolute path of the ledger the answer was read from.
    pub runlog_path: String,
    /// Paired runs in the ledger, in-flight and completed together.
    pub total_entries: usize,
    /// Runs with an intent but no verdict yet, newest first — in flight right
    /// now, or lost (`gantry doctor` distinguishes the two).
    pub in_flight: Vec<RunJson>,
    /// The most recent completed runs, newest first, up to the limit.
    pub recent: Vec<RunJson>,
    /// Unparseable ledger lines skipped (torn final line, truncated record).
    pub skipped_lines: usize,
    /// Verdict records whose `run_id` matched no intent.
    pub unmatched_verdicts: usize,
}

/// Build the document from a ledger already read. Pure — tests fabricate
/// ledgers; `cli` is the only I/O.
pub fn build(ledger: Ledger, runlog_path: String, limit: usize) -> StatusDoc {
    let total_entries = ledger.entries.len();
    // File order is arrival order (append-only), so newest = last; both lists
    // read newest-first because the top of the list is the run an agent
    // asking "what is happening right now" cares about.
    let newest_first = ledger.entries.iter().rev();
    let in_flight: Vec<RunJson> = newest_first
        .clone()
        .filter(|entry| entry.verdict.is_none())
        .map(RunJson::from_entry)
        .collect();
    let recent: Vec<RunJson> = newest_first
        .filter(|entry| entry.verdict.is_some())
        .take(limit)
        .map(RunJson::from_entry)
        .collect();
    StatusDoc {
        schema_version: SCHEMA_VERSION,
        runlog_path,
        total_entries,
        in_flight,
        recent,
        skipped_lines: ledger.skipped_lines,
        unmatched_verdicts: ledger.unmatched_verdicts,
    }
}

/// First eight characters of a run id for the listing. Ids are minted per
/// invocation and never referenced across boxes, so a prefix is enough to
/// spot a run; `why --json` carries the full id.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Human rendering — a rendering of the JSON document, never a second source
/// of truth.
pub fn render_text(doc: &StatusDoc) -> String {
    let mut lines = vec![format!("gantry status — {}", doc.runlog_path)];
    let mut header = format!(
        "{} runs · {} in flight · {} most recent",
        doc.total_entries,
        doc.in_flight.len(),
        doc.recent.len()
    );
    if doc.skipped_lines > 0 {
        header.push_str(&format!(
            " · {} unparseable ledger lines skipped",
            doc.skipped_lines
        ));
    }
    lines.push(header);

    for run in doc.in_flight.iter().chain(doc.recent.iter()) {
        let outcome = match &run.verdict {
            Some(verdict) => format!(
                "{} ({}, exit {})",
                verdict.verdict,
                ran_name(verdict.ran),
                verdict.exit_code
            ),
            None => format!(
                "IN FLIGHT ({}/{})",
                decision_name(run.decision),
                run.backend
            ),
        };
        lines.push(format!(
            "{} {} · {} {} → {}",
            short_id(&run.run_id),
            iso8601_utc(run.ts),
            run.tool,
            run.args.join(" "),
            outcome
        ));
    }
    if doc.in_flight.is_empty() && doc.recent.is_empty() {
        lines.push("no runs recorded yet".to_string());
    }
    lines.join("\n") + "\n"
}

/// Entry point from main.rs: `gantry status [--json] [--limit N]` (the
/// leading `gantry status` already stripped from `argv`).
pub fn cli(argv: &[String]) -> u8 {
    const USAGE: &str = "usage: gantry status [--json] [--limit N]";

    let mut json = false;
    let mut limit = DEFAULT_LIMIT;
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--json" => json = true,
            "--limit" => {
                let Some(value) = argv.get(index + 1) else {
                    eprintln!("gantry status: --limit needs a number");
                    eprintln!("{USAGE}");
                    return 2;
                };
                match value.parse::<usize>() {
                    Ok(parsed) => limit = parsed,
                    Err(_) => {
                        eprintln!("gantry status: --limit '{value}' is not a number");
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
                index += 1;
            }
            other => {
                eprintln!("gantry status: unexpected argument '{other}'");
                eprintln!("{USAGE}");
                return 2;
            }
        }
        index += 1;
    }

    let document = match RunLog::open() {
        Ok(runlog) => {
            let path = runlog.path().display().to_string();
            match runlog.read_entries() {
                Ok(ledger) => Some(build(ledger, path, limit)),
                Err(e) => {
                    eprintln!("gantry status: cannot read ledger: {e}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("gantry status: cannot open ledger: {e}");
            None
        }
    };

    // EC-08 for diagnostics: an unreadable ledger is a status worth
    // reporting (the reason is already on stderr); the command answered
    // with an empty list, which is not a failure — nothing is printed.
    if let Some(doc) = document {
        if json {
            match serde_json::to_string_pretty(&doc) {
                Ok(text) => println!("{text}"),
                Err(e) => eprintln!("gantry status: cannot serialize document: {e}"),
            }
        } else {
            print!("{}", render_text(&doc));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{schema, STATUS_SCHEMA};
    use crate::runlog::{
        Decision, GateInputs, IntentRecord, RanLocation, RunEntry, Verdict, VerdictRecord,
    };
    use std::path::PathBuf;

    const FAKE_LEDGER: &str = "/tmp/gantry-test/runs.jsonl";

    fn entry(verdict: Option<Verdict>) -> RunEntry {
        let intent = IntentRecord::new(
            "cargo".to_string(),
            vec!["test".to_string()],
            "https://github.com/example/repo".to_string(),
            "abc123".to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean: true,
            },
            Decision::Remote,
            String::new(),
            "command".to_string(),
        );
        let verdict = verdict.map(|verdict| {
            VerdictRecord::new(
                intent.run_id.clone(),
                verdict,
                RanLocation::Remote,
                0,
                "gantry-x7k2p".to_string(),
                None,
            )
        });
        RunEntry { intent, verdict }
    }

    fn ledger(entries: Vec<RunEntry>) -> Ledger {
        Ledger {
            entries,
            skipped_lines: 2,
            unmatched_verdicts: 1,
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

    /// Validate a document against the published status schema.
    fn assert_valid(doc: &StatusDoc) {
        let schema: serde_json::Value =
            serde_json::from_str(STATUS_SCHEMA).expect("published schema parses");
        let doc = serde_json::to_value(doc).expect("document serializes");
        schema::validate(&schema, &doc)
            .unwrap_or_else(|errors| panic!("document violates its published schema: {errors:?}"));
    }

    #[test]
    fn in_flight_and_recent_are_newest_first() {
        // File order (oldest first): completed, in-flight, completed.
        let doc = build(
            ledger(vec![
                entry(Some(Verdict::Pass)),
                entry(None),
                entry(Some(Verdict::TestFailure)),
            ]),
            FAKE_LEDGER.to_string(),
            DEFAULT_LIMIT,
        );

        assert_eq!(doc.total_entries, 3);
        assert_eq!(doc.in_flight.len(), 1);
        // The in-flight entry is the middle one; newest-first recent puts the
        // last entry (TestFailure) above the first (Pass).
        assert_eq!(doc.recent.len(), 2);
        assert_eq!(
            doc.recent[0].verdict.as_ref().map(|v| v.verdict),
            Some(Verdict::TestFailure)
        );
        assert_eq!(
            doc.recent[1].verdict.as_ref().map(|v| v.verdict),
            Some(Verdict::Pass)
        );
    }

    #[test]
    fn the_limit_bounds_recent_only() {
        let doc = build(
            ledger(vec![
                entry(Some(Verdict::Pass)),
                entry(Some(Verdict::Pass)),
                entry(Some(Verdict::Pass)),
                entry(None),
            ]),
            FAKE_LEDGER.to_string(),
            2,
        );
        assert_eq!(doc.recent.len(), 2);
        assert_eq!(doc.in_flight.len(), 1);
        assert_eq!(doc.total_entries, 4);
    }

    #[test]
    fn status_json_validates_against_its_published_schema() {
        let populated = build(
            // Arrival order (oldest first): the classified failure landed
            // last, so it tops the newest-first `recent` list.
            ledger(vec![
                entry(Some(Verdict::Pass)),
                entry(None),
                classified(
                    entry(Some(Verdict::TestFailure)),
                    crate::verdict::FailureClass::TestFailure,
                ),
            ]),
            FAKE_LEDGER.to_string(),
            DEFAULT_LIMIT,
        );
        let empty = build(ledger(vec![]), FAKE_LEDGER.to_string(), DEFAULT_LIMIT);
        assert_valid(&populated);
        assert_valid(&empty);

        // Ledger health survives into the document.
        assert_eq!(populated.skipped_lines, 2);
        assert_eq!(populated.unmatched_verdicts, 1);

        // The class spellings are the ones the schema's enum describes.
        let doc = serde_json::to_value(&populated).unwrap();
        assert_eq!(doc["recent"][0]["failure_class"], "test-failure");
        assert_eq!(doc["recent"][1]["failure_class"], serde_json::Value::Null);
        assert_eq!(
            doc["in_flight"][0]["failure_class"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn failure_class_is_projected_straight_off_the_verdict_record() {
        let doc = build(
            // Same arrival order: the classified failure is the newest
            // completed run.
            ledger(vec![
                entry(Some(Verdict::Pass)),
                entry(None),
                classified(
                    entry(Some(Verdict::TestFailure)),
                    crate::verdict::FailureClass::CompileError,
                ),
            ]),
            FAKE_LEDGER.to_string(),
            DEFAULT_LIMIT,
        );

        // Newest-first recent: the class-bearing run is on top; each run
        // carries its own record's class, and only that.
        assert_eq!(doc.recent.len(), 2);
        assert_eq!(
            doc.recent[0].failure_class,
            Some(crate::verdict::FailureClass::CompileError)
        );
        assert_eq!(doc.recent[1].failure_class, None);
        // In-flight runs have no verdict record, hence no class.
        assert_eq!(doc.in_flight.len(), 1);
        assert_eq!(doc.in_flight[0].failure_class, None);
    }

    #[test]
    fn a_pre_taxonomy_record_reads_as_a_null_class_not_an_error() {
        // The pre-taxonomy producer shape: schema 1, no `failure_class` key
        // on disk. The ledger parse is lenient upstream; status must read it
        // as null and list the run like any other.
        let legacy: VerdictRecord = serde_json::from_str(
            r#"{"rec":"verdict","schema_version":1,"run_id":"legacy","ts":2,"verdict":"test_failure","ran":"remote","exit_code":101,"handle":"gantry-x7k2p"}"#,
        )
        .expect("legacy record parses");
        assert!(legacy.failure_class.is_none());

        let doc = build(
            ledger(vec![{
                // Pair the intent with the legacy verdict the way the ledger
                // would: same run_id.
                let mut intent = entry(Some(Verdict::Pass)).intent;
                intent.run_id = legacy.run_id.clone();
                RunEntry {
                    intent,
                    verdict: Some(legacy),
                }
            }]),
            FAKE_LEDGER.to_string(),
            DEFAULT_LIMIT,
        );

        assert_eq!(doc.recent.len(), 1);
        assert_eq!(doc.recent[0].failure_class, None);
        // The field is emitted — null, not absent — so the document still
        // validates against the published schema's required list.
        assert_valid(&doc);
        let serialized = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            serialized["recent"][0]["failure_class"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn human_text_lists_in_flight_runs_before_completed_ones() {
        let doc = build(
            ledger(vec![entry(Some(Verdict::Pass)), entry(None)]),
            FAKE_LEDGER.to_string(),
            DEFAULT_LIMIT,
        );
        let text = render_text(&doc);
        let in_flight = text.find("IN FLIGHT").expect("in-flight line present");
        let completed = text.find("Pass").expect("completed line present");
        assert!(in_flight < completed, "newest-first: {text}");
    }

    #[test]
    fn human_text_says_when_the_ledger_is_empty() {
        let doc = build(ledger(vec![]), FAKE_LEDGER.to_string(), DEFAULT_LIMIT);
        assert!(render_text(&doc).contains("no runs recorded yet"));
    }

    #[test]
    fn usage_errors_exit_two_before_any_ledger_is_touched() {
        let argv: Vec<String> = vec!["--limit".to_string()];
        assert_eq!(cli(&argv), 2);

        let argv: Vec<String> = vec!["--limit".to_string(), "soon".to_string()];
        assert_eq!(cli(&argv), 2);

        let argv: Vec<String> = vec!["--watch".to_string()];
        assert_eq!(cli(&argv), 2);
    }
}
