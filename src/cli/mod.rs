// gantry — management CLI diagnostics: `why`, `explain`, `status`
// (plan §"CLI surface").
//
// Diagnostics here are agent-first (plan, same section): `why`, `explain`,
// and `status` all take `--json`, the JSON surface is the primary interface,
// and human text is a rendering of it — a NEEDLE worker that hits a
// degradation branches on `why --json` fields instead of parsing logs or
// stderr (AS-2). Every document carries a top-level `schema_version`, and the
// published schemas live in `docs/schemas/` (why-v1.json, explain-v1.json,
// status-v1.json) — [`crate::cli::schema`] validates documents against them,
// which is what keeps the published files load-bearing rather than decorative
// (acceptance for bf-1n4: "`why --json` validates against its published
// schema").
//
// Versioning stance is the plan's (§"Versioning & compatibility"): additive
// fields are free and consumers ignore unknown ones; removing or retyping a
// field is a major bump of [`SCHEMA_VERSION`] and a new schema file.

pub mod explain;
pub mod init; // plan §8: `gantry init --ssh <target>` — SSH-first onboarding (verify, install, preset, doctor --e2e)
pub mod run;
pub mod schema;
pub mod status;
pub mod why;

use crate::runlog::{Durations, GateInputs, RanLocation, RunEntry, Verdict, VerdictRecord};
use serde::{Deserialize, Serialize};

/// Schema version of the `--json` output contract for `why`/`explain`/
/// `status`. One number governs all three documents: they evolve together
/// (the plan versioning section treats the three as one `--json` surface).
pub const SCHEMA_VERSION: u32 = 1;

/// The published schemas these documents validate against (plan §"Versioning
/// & compatibility"; the acceptance for bf-1n4 is that `why --json`
/// validates against its published schema). Embedded at build time so the
/// single static binary's tests always read the exact text that ships in
/// `docs/schemas/` — the tests run real command output through
/// [`schema::validate`] against these, which is what keeps the published
/// files load-bearing rather than decorative.
pub const WHY_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/schemas/why-v1.json"
));

/// `gantry explain --json`'s published schema.
pub const EXPLAIN_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/schemas/explain-v1.json"
));

/// `gantry status --json`'s published schema.
pub const STATUS_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/schemas/status-v1.json"
));

/// The `--json` shape of one recorded run — `why`'s `run` object and the
/// entry shape of `status`'s arrays. Projected from the ledger's
/// [`RunEntry`]: ledger-internal fields (`rec`, the record-level
/// `schema_version`, the per-record `ts` of the verdict half) stay ledger
/// business; this is the diagnostics contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunJson {
    /// The run id from the intent record.
    pub run_id: String,
    /// Unix milliseconds when the intent was written (run start).
    pub ts: u64,
    /// Tool profile (e.g. "cargo").
    pub tool: String,
    /// The intercepted argv tail, subcommand first.
    pub args: Vec<String>,
    /// Repository URL as recorded (credentials stripped per S-5).
    pub repo: String,
    /// Commit sha the run was for.
    pub sha: String,
    /// Caller's directory relative to the repo root.
    pub cwd_rel: std::path::PathBuf,
    /// Gate inputs as captured at decision time.
    pub gate: GateJson,
    /// Where the run was sent ("remote" / "local").
    pub decision: crate::runlog::Decision,
    /// Why the decision was made, as the pipeline recorded it. Empty on the
    /// eligible-remote path (the pipeline records no reason there — the run
    /// simply proceeded).
    pub reason: String,
    /// Backend chosen ("argo" / "command" / "none").
    pub backend: String,
    /// Terminal verdict when one landed; `null` while the run is in flight
    /// or lost (SIGKILL, crash). Agents branch on `verdict.verdict` — the
    /// AS-2 failure class — without parsing logs.
    pub verdict: Option<VerdictJson>,
    /// True when the intent never got a verdict: in flight, or a lost run
    /// (`gantry doctor` reports the latter as an orphan).
    pub orphaned: bool,
}

impl RunJson {
    /// Project a ledger entry into the diagnostics shape.
    pub fn from_entry(entry: &RunEntry) -> Self {
        RunJson {
            run_id: entry.intent.run_id.clone(),
            ts: entry.intent.ts,
            tool: entry.intent.tool.clone(),
            args: entry.intent.args.clone(),
            repo: entry.intent.repo.clone(),
            sha: entry.intent.sha.clone(),
            cwd_rel: entry.intent.cwd_rel.clone(),
            gate: GateJson::from(&entry.intent.gate),
            decision: entry.intent.decision,
            reason: entry.intent.reason.clone(),
            backend: entry.intent.backend.clone(),
            verdict: entry.verdict.as_ref().map(VerdictJson::from),
            orphaned: entry.verdict.is_none(),
        }
    }
}

/// Gate inputs as the ledger captured them (plan §7: "Intent records capture
/// all gate inputs … so `gantry why` can replay the decision truthfully").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateJson {
    /// Inside a git work tree.
    pub worktree: bool,
    /// HEAD resolves (not unborn).
    pub head: bool,
    /// Configured ci_remote exists.
    pub remote: bool,
    /// Working tree clean (porcelain empty).
    pub clean: bool,
}

impl From<&GateInputs> for GateJson {
    fn from(gate: &GateInputs) -> Self {
        GateJson {
            worktree: gate.worktree,
            head: gate.head,
            remote: gate.remote,
            clean: gate.clean,
        }
    }
}

/// Terminal verdict projection: the fields an agent branches on, with the
/// ledger-internal bookkeeping (`rec`, run-level `schema_version`, the
/// verdict record's own `run_id`/`ts`) left out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerdictJson {
    /// The failure class (AS-2): "pass" / "test_failure" / "gate_failure" /
    /// "infra_failure" / "cancelled" / "superseded".
    pub verdict: Verdict,
    /// Where the run actually executed.
    pub ran: RanLocation,
    /// The faithful exit code the caller saw (INV-3).
    pub exit_code: i32,
    /// Backend handle (workflow name, "local", …).
    pub handle: String,
    /// Duration breakdown when the pipeline recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durations_ms: Option<Durations>,
}

impl From<&VerdictRecord> for VerdictJson {
    fn from(record: &VerdictRecord) -> Self {
        VerdictJson {
            verdict: record.verdict,
            ran: record.ran,
            exit_code: record.exit_code,
            handle: record.handle.clone(),
            durations_ms: record.durations_ms.clone(),
        }
    }
}

/// Render the gate trace line: every check with YES/NO, the first NO marked
/// `← here` — the plan's replay example is literally "clean tree: NO ← here",
/// pointing at the check that flipped the decision. Shared by `why` (replay)
/// and `explain` (dry run) so the two read identically.
pub fn gate_trace_line(gate: &GateJson) -> String {
    let checks = [
        ("work tree", gate.worktree),
        ("HEAD", gate.head),
        ("remote", gate.remote),
        ("clean tree", gate.clean),
    ];

    let mut parts = Vec::with_capacity(checks.len());
    let mut marked = false;
    for (name, ok) in checks {
        let mut part = format!("{name} {}", if ok { "YES" } else { "NO" });
        if !ok && !marked {
            part.push_str(" ← here");
            marked = true;
        }
        parts.push(part);
    }
    parts.join(" · ")
}

/// The backend's config spelling for transcripts and JSON ("argo" beats
/// `Argo` — the same convention quickcheck uses for its header line).
pub fn backend_name(backend: crate::config::Backend) -> &'static str {
    match backend {
        crate::config::Backend::None => "none",
        crate::config::Backend::Argo => "argo",
        crate::config::Backend::Command => "command",
    }
}

/// The serde wire spelling of a decision ("remote"/"local") for human text —
/// the same strings the `--json` contract uses, so a transcript and the
/// document it renders can never disagree about a name.
pub fn decision_name(decision: crate::runlog::Decision) -> &'static str {
    match decision {
        crate::runlog::Decision::Remote => "remote",
        crate::runlog::Decision::Local => "local",
    }
}

/// The serde wire spelling of a run location, same stance as
/// [`decision_name`].
pub fn ran_name(ran: crate::runlog::RanLocation) -> &'static str {
    match ran {
        crate::runlog::RanLocation::Remote => "remote",
        crate::runlog::RanLocation::Local => "local",
        crate::runlog::RanLocation::LocalAfterInfra => "local_after_infra",
    }
}

/// Format unix milliseconds as ISO 8601 UTC (`1970-01-01T00:00:00Z`).
///
/// Diagnostics timestamps read better absolute than as bare epoch numbers,
/// and the fleet reads UTC; the conversion is the standard days-to-civil
/// algorithm (Hinnant), no calendar crate.
pub fn iso8601_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar. Howard Hinnant's `civil_from_days`, the well-known closed form.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runlog::{Decision, GateInputs};
    use std::path::PathBuf;

    fn gate(worktree: bool, head: bool, remote: bool, clean: bool) -> GateJson {
        GateJson::from(&GateInputs {
            worktree,
            head,
            remote,
            clean,
        })
    }

    #[test]
    fn gate_trace_marks_the_first_no_with_here() {
        let line = gate_trace_line(&gate(true, true, true, false));
        assert_eq!(
            line,
            "work tree YES · HEAD YES · remote YES · clean tree NO ← here"
        );
    }

    #[test]
    fn gate_trace_marks_only_the_first_no() {
        let line = gate_trace_line(&gate(false, false, true, false));
        assert_eq!(
            line,
            "work tree NO ← here · HEAD NO · remote YES · clean tree NO"
        );
    }

    #[test]
    fn gate_trace_all_yes_carries_no_marker() {
        let line = gate_trace_line(&gate(true, true, true, true));
        assert_eq!(
            line,
            "work tree YES · HEAD YES · remote YES · clean tree YES"
        );
        assert!(!line.contains('←'));
    }

    #[test]
    fn run_json_projects_a_ledger_entry() {
        let intent = crate::runlog::IntentRecord::new(
            "cargo".to_string(),
            vec!["test".to_string()],
            "https://example.com/repo".to_string(),
            "abc123".to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean: false,
            },
            Decision::Local,
            "dirty".to_string(),
            "none".to_string(),
        );
        let entry = RunEntry {
            intent,
            verdict: None,
        };

        let run = RunJson::from_entry(&entry);
        assert!(run.orphaned);
        assert!(run.verdict.is_none());
        assert!(!run.gate.clean);
        assert_eq!(run.decision, Decision::Local);
        assert_eq!(run.reason, "dirty");

        // The serialized shape is what the published schema describes: the
        // enums in their snake_case wire spelling, cwd_rel as a string.
        let doc = serde_json::to_value(&run).unwrap();
        assert_eq!(doc["decision"], "local");
        assert_eq!(doc["verdict"], serde_json::Value::Null);
        assert_eq!(doc["cwd_rel"], ".");
        assert_eq!(doc["gate"]["clean"], false);
    }

    #[test]
    fn verdict_json_keeps_durations_optional() {
        let with = VerdictRecord::new(
            "r1".to_string(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "gantry-x7k2p".to_string(),
            Some(Durations {
                gate: 40,
                push: 900,
                queue: 12_000,
                run: 341_000,
            }),
        );
        let doc = serde_json::to_value(VerdictJson::from(&with)).unwrap();
        assert_eq!(doc["verdict"], "pass");
        assert_eq!(doc["ran"], "remote");
        assert_eq!(doc["durations_ms"]["queue"], 12_000);

        let without = VerdictRecord::new(
            "r2".to_string(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "h".to_string(),
            None,
        );
        let doc = serde_json::to_value(VerdictJson::from(&without)).unwrap();
        assert!(doc.get("durations_ms").is_none(), "{doc}");
    }

    #[test]
    fn iso8601_formats_the_epoch_and_a_known_instant() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601_utc(1_789_000_000_000), "2026-09-10T00:26:40Z");
    }

    #[test]
    fn iso8601_handles_leap_years_and_milliseconds_below_a_second() {
        // 2024-02-29T12:20:00.5Z — the leap day itself.
        let ms = 1_709_209_200_500;
        assert_eq!(iso8601_utc(ms), "2024-02-29T12:20:00Z");
    }

    #[test]
    fn backend_name_uses_the_config_spelling() {
        assert_eq!(backend_name(crate::config::Backend::None), "none");
        assert_eq!(backend_name(crate::config::Backend::Argo), "argo");
        assert_eq!(backend_name(crate::config::Backend::Command), "command");
    }

    #[test]
    fn published_schemas_parse_and_pin_schema_version_one() {
        for (name, text) in [
            ("why", WHY_SCHEMA),
            ("explain", EXPLAIN_SCHEMA),
            ("status", STATUS_SCHEMA),
        ] {
            let schema: serde_json::Value =
                serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                schema["properties"]["schema_version"]["const"], 1,
                "{name} must pin the contract version it publishes"
            );
        }
    }

    /// The validator degrades to loose acceptance on keywords it does not
    /// know, so a schema edited to use an unsupported construct would
    /// silently stop being load-bearing. Hold the published files to the
    /// documented subset: the semantic keywords [`crate::cli::schema`]
    /// implements, plus the descriptive ones that carry no validation
    /// semantics ($schema/title/description/$comment).
    ///
    /// Object keys *inside* `properties` are document property names, not
    /// keywords — their values are the subschemas to recurse into — and
    /// `enum`/`const` hold document values, not schemas, so neither is
    /// walked as keyword-bearing.
    #[test]
    fn published_schemas_use_only_the_supported_keyword_subset() {
        const SEMANTIC: [&str; 7] = [
            "type",
            "enum",
            "const",
            "properties",
            "required",
            "additionalProperties",
            "items",
        ];
        const DESCRIPTIVE: [&str; 4] = ["$schema", "$comment", "title", "description"];

        fn walk(value: &serde_json::Value, violations: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, nested) in map {
                        if !SEMANTIC.contains(&key.as_str()) && !DESCRIPTIVE.contains(&key.as_str())
                        {
                            violations.push(key.clone());
                        }
                        match key.as_str() {
                            // The keys of a `properties` object name the
                            // document's fields; only their subschemas can
                            // carry keywords.
                            "properties" => {
                                if let Some(subschemas) = nested.as_object() {
                                    for subschema in subschemas.values() {
                                        walk(subschema, violations);
                                    }
                                }
                            }
                            // Document values, never schemas.
                            "enum" | "const" => {}
                            _ => walk(nested, violations),
                        }
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, violations);
                    }
                }
                _ => {}
            }
        }

        for (name, text) in [
            ("why", WHY_SCHEMA),
            ("explain", EXPLAIN_SCHEMA),
            ("status", STATUS_SCHEMA),
        ] {
            let schema: serde_json::Value =
                serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let mut violations = Vec::new();
            walk(&schema, &mut violations);
            assert!(
                violations.is_empty(),
                "{name} uses keywords outside the supported subset: {violations:?}"
            );
        }
    }
}
