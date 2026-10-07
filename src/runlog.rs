// gantry — write-ahead RunLog (intent + verdict records) (plan Component 7).
//
// Phase 1a: write-ahead logging with orphan detection (bf-41t).
//
// This module defines:
// - RunLog: write-ahead ledger for every invocation (runs.jsonl)
// - IntentRecord: OPEN intent record appended BEFORE dispatch, capturing gate inputs
// - VerdictRecord: terminal record appended on every exit path
// - Orphan detection: query to find lost runs (SIGKILL mid-run, INV-1)
//
// The write-ahead form guarantees that every gantry invocation either:
// 1. Completes normally (intent + verdict pair), or
// 2. Leaves an orphaned intent that doctor reports (detectable silent skip)
//
// Concurrency safety: O_APPEND single-line writes, file-level locking on open.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current schema version for runs.jsonl records.
/// Increment this on breaking changes; readers MUST tolerate newer minor versions.
pub const SCHEMA_VERSION: u32 = 1;

/// RunLog: write-ahead ledger for every gantry invocation.
///
/// Lives at `~/.local/state/gantry/runs.jsonl` (plan Component 7, "RunLog & UX").
/// Every invocation writes an OPEN intent record BEFORE dispatch, and every
/// exit path MUST write a matching terminal verdict record.
///
/// Orphaned OPEN records (no matching verdict) indicate a lost run (INV-1):
/// gantry process SIGKILLed mid-run, crash, or similar catastrophe.
pub struct RunLog {
    /// Path to the runs.jsonl file (~/.local/state/gantry/runs.jsonl).
    log_path: PathBuf,
}

impl RunLog {
    /// Open the runlog at `~/.local/state/gantry/runs.jsonl`.
    ///
    /// Creates the state directory if it doesn't exist. The file is opened
    /// with O_APPEND for concurrency-safe single-line writes.
    ///
    /// Returns Err if the state directory cannot be created or the file
    /// cannot be opened for append. Per plan failure mode EC-08, gantry
    /// MUST proceed with in-memory records if the log is unwritable.
    pub fn open() -> Result<Self, RunLogError> {
        let state_dir = Self::state_dir().ok_or(RunLogError::CannotDetermineStateDir)?;

        // Create state directory if it doesn't exist
        std::fs::create_dir_all(&state_dir).map_err(|e| RunLogError::CannotCreateStateDir {
            path: state_dir.clone(),
            source: e,
        })?;

        let log_path = state_dir.join("runs.jsonl");

        Ok(RunLog { log_path })
    }

    /// Open a runlog at an explicit state directory — the test seam that
    /// keeps environment redirection out of the test process (a unit test
    /// cannot export HOME, so it points the log at its temp dir instead;
    /// same explicit-path convention as `check_git_gate_in` and
    /// `RefPusher::push_in`).
    #[cfg(test)]
    pub(crate) fn open_in(state_dir: &std::path::Path) -> Self {
        RunLog {
            log_path: state_dir.join("runs.jsonl"),
        }
    }

    /// Write an OPEN intent record BEFORE dispatch.
    ///
    /// This MUST be called before any real work happens (gate checks passed,
    /// backend chosen, about to push refs or submit). The intent captures all
    /// gate inputs so `gantry why` can replay the decision truthfully.
    ///
    /// Uses O_APPEND single-line writes for concurrency safety.
    ///
    /// Returns the run_id that MUST be used in the matching verdict record.
    /// Returns Err if the write fails (full disk, permissions, etc.).
    pub fn open_intent(&self, intent: &IntentRecord) -> Result<String, RunLogError> {
        let run_id = intent.run_id.clone();

        // Serialize to JSON (single line, no pretty-print)
        let json = serde_json::to_string(intent).map_err(|e| RunLogError::Serialization {
            context: "intent record".to_string(),
            source: e,
        })?;

        // Append to log with newline (O_APPEND for concurrency)
        self.append_line(&json)?;

        Ok(run_id)
    }

    /// Write a terminal verdict record on every exit path.
    ///
    /// This MUST be called on EVERY exit path (pass, fail, fallback, cancel,
    /// crash handler). The verdict completes the intent-verdict pair; missing
    /// verdicts are orphaned intents that doctor reports.
    ///
    /// Uses O_APPEND single-line writes for concurrency safety.
    ///
    /// Returns Err if the write fails. Per failure mode EC-08, gantry MUST
    /// print a warning but MUST NOT block on verdict write failures.
    pub fn close_verdict(&self, verdict: &VerdictRecord) -> Result<(), RunLogError> {
        // Serialize to JSON (single line, no pretty-print)
        let json = serde_json::to_string(verdict).map_err(|e| RunLogError::Serialization {
            context: "verdict record".to_string(),
            source: e,
        })?;

        // Append to log with newline (O_APPEND for concurrency)
        self.append_line(&json)?;

        Ok(())
    }

    /// Find orphaned OPEN intent records (INV-1).
    ///
    /// Reads runs.jsonl and tracks run_ids in intent/verdict sets. Returns
    /// run_ids that have an intent but no matching verdict — these are lost
    /// runs from SIGKILL mid-run, crashes, or similar catastrophes.
    ///
    /// Used by `gantry doctor` to report silently-skipped runs.
    ///
    /// Returns Err if the log cannot be read. Empty result means no orphans.
    pub fn find_orphans(&self) -> Result<Vec<OrphanedRun>, RunLogError> {
        if !self.log_path.exists() {
            // No log file = no orphans
            return Ok(Vec::new());
        }

        let content =
            std::fs::read_to_string(&self.log_path).map_err(|e| RunLogError::CannotReadLog {
                path: self.log_path.clone(),
                source: e,
            })?;

        let mut intents: std::collections::HashMap<String, IntentRecord> =
            std::collections::HashMap::new();
        let mut verdicts: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (line_num, line) in content.lines().enumerate() {
            if line.is_empty() {
                continue;
            }

            // Parse the line to determine record type
            let maybe_rec = serde_json::from_str::<serde_json::Value>(line);
            let rec = maybe_rec.map_err(|e| RunLogError::CorruptRecord {
                line_num: line_num + 1,
                source: e,
            })?;

            let record_type = rec.get("rec").and_then(|v| v.as_str()).ok_or_else(|| {
                RunLogError::CorruptRecord {
                    line_num: line_num + 1,
                    source: serde::de::Error::custom("missing 'rec' field"),
                }
            })?;

            let run_id = rec
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RunLogError::CorruptRecord {
                    line_num: line_num + 1,
                    source: serde::de::Error::custom("missing 'run_id' field"),
                })?
                .to_string();

            match record_type {
                "intent" => {
                    let intent: IntentRecord =
                        serde_json::from_str(line).map_err(|e| RunLogError::CorruptRecord {
                            line_num: line_num + 1,
                            source: e,
                        })?;
                    intents.insert(run_id, intent);
                }
                "verdict" => {
                    verdicts.insert(run_id);
                }
                _ => {
                    return Err(RunLogError::CorruptRecord {
                        line_num: line_num + 1,
                        source: serde::de::Error::custom(format!(
                            "unknown record type: {}",
                            record_type
                        )),
                    });
                }
            }
        }

        // Find intents with no matching verdict
        let orphans = intents
            .into_iter()
            .filter(|(run_id, _)| !verdicts.contains(run_id))
            .map(|(run_id, intent)| OrphanedRun {
                run_id,
                intent,
                orphaned_at: SystemTime::now(),
            })
            .collect();

        Ok(orphans)
    }

    /// Read the whole ledger as paired runs, oldest entry first.
    ///
    /// This is the read side of the write-ahead ledger — the source `gantry
    /// why` replays the last run from and `gantry status` lists recent and
    /// in-flight runs from. Every intent is paired with the newest verdict
    /// carrying its `run_id`; an intent with no verdict is an in-flight or
    /// lost run (what [`RunLog::find_orphans`] reports for doctor).
    ///
    /// Corrupt lines (a torn final line after a crash mid-write, a truncated
    /// record from a full disk) are skipped and counted rather than fatal: a
    /// damaged ledger must not brick the diagnostics that exist to explain
    /// damaged runs. This is deliberately more forgiving than
    /// [`RunLog::find_orphans`], which hard-errors — doctor must not bless a
    /// ledger it only half understood, while `why`/`status` degrade to
    /// answering from the records that did parse. Verdicts whose `run_id`
    /// matches no intent (the degenerate ineligible path in the decision
    /// engine writes one) have no run to attach to and are counted in
    /// [`Ledger::unmatched_verdicts`].
    pub fn read_entries(&self) -> Result<Ledger, RunLogError> {
        let mut ledger = Ledger {
            entries: Vec::new(),
            skipped_lines: 0,
            unmatched_verdicts: 0,
        };

        if !self.log_path.exists() {
            // No log file = no runs; an empty ledger is not an error.
            return Ok(ledger);
        }

        let content =
            std::fs::read_to_string(&self.log_path).map_err(|e| RunLogError::CannotReadLog {
                path: self.log_path.clone(),
                source: e,
            })?;

        for line in content.lines() {
            if line.is_empty() {
                continue;
            }

            // Classify by the record discriminator before typed parsing so an
            // unknown or missing `rec` degrades to a skipped line instead of
            // a parse error shaped like a panic.
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
                ledger.skipped_lines += 1;
                continue;
            };

            match rec.get("rec").and_then(|v| v.as_str()) {
                Some("intent") => match serde_json::from_str::<IntentRecord>(line) {
                    Ok(intent) => ledger.entries.push(RunEntry {
                        intent,
                        verdict: None,
                    }),
                    Err(_) => ledger.skipped_lines += 1,
                },
                Some("verdict") => match serde_json::from_str::<VerdictRecord>(line) {
                    Ok(verdict) => {
                        // Newest intent wins: run_ids are minted per-invocation,
                        // so a match is unique in practice, and a duplicate would
                        // mean a replayed id — pairing with the latest keeps the
                        // last-run view consistent with the append-only tail.
                        let matched = ledger
                            .entries
                            .iter_mut()
                            .rev()
                            .find(|entry| entry.intent.run_id == verdict.run_id);
                        match matched {
                            Some(entry) => entry.verdict = Some(verdict),
                            None => ledger.unmatched_verdicts += 1,
                        }
                    }
                    Err(_) => ledger.skipped_lines += 1,
                },
                _ => ledger.skipped_lines += 1,
            }
        }

        Ok(ledger)
    }

    /// Get the path to the runlog file (for display/debugging).
    pub fn path(&self) -> &Path {
        &self.log_path
    }

    /// Append a single line to the log file with O_APPEND.
    ///
    /// Opens the file for append, writes the line, adds a newline, and flushes.
    /// O_APPEND ensures concurrent writes don't clobber each other.
    fn append_line(&self, line: &str) -> Result<(), RunLogError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| RunLogError::CannotAppend {
                path: self.log_path.clone(),
                source: e,
            })?;

        writeln!(file, "{}", line)
            .and_then(|_| file.flush())
            .map_err(|e| RunLogError::CannotAppend {
                path: self.log_path.clone(),
                source: e,
            })?;

        Ok(())
    }

    /// Determine the state directory path (~/.local/state/gantry/).
    ///
    /// Returns None if HOME is not set (should not happen in normal use).
    fn state_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("gantry")
        })
    }
}

/// Intent record: OPEN intent written BEFORE dispatch.
///
/// Captures all gate inputs (tree state, remote, kill-switch state, chosen
/// backend) so `gantry why` can replay the decision truthfully after the fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentRecord {
    /// Record type discriminator ("intent").
    #[serde(rename = "rec")]
    pub record_type: String,

    /// Schema version for backward compatibility.
    #[serde(rename = "schema_version")]
    pub schema_version: u32,

    /// Unique run identifier (short UUID-like, e.g., "01J...").
    pub run_id: String,

    /// Unix timestamp (milliseconds) when intent was recorded.
    pub ts: u64,

    /// Tool being invoked (e.g., "cargo").
    pub tool: String,

    /// Arguments to the tool (e.g., ["test", "--", "--nocapture"]).
    pub args: Vec<String>,

    /// Repository URL (redacted of credentials per S-5).
    pub repo: String,

    /// Commit SHA being run.
    pub sha: String,

    /// Caller's directory relative to repo root (for workspace-member invocations).
    pub cwd_rel: PathBuf,

    /// Gate inputs: all GitGate checks.
    pub gate: GateInputs,

    /// Decision: where this run will execute.
    pub decision: Decision,

    /// Reason for the decision (human-readable, for `gantry why`).
    pub reason: String,

    /// Backend chosen for this run (argo/command/none).
    pub backend: String,
}

impl IntentRecord {
    /// Create a new intent record.
    ///
    /// Generates a run_id and timestamp automatically.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool: String,
        args: Vec<String>,
        repo: String,
        sha: String,
        cwd_rel: PathBuf,
        gate: GateInputs,
        decision: Decision,
        reason: String,
        backend: String,
    ) -> Self {
        IntentRecord {
            record_type: "intent".to_string(),
            schema_version: SCHEMA_VERSION,
            run_id: Self::generate_run_id(),
            ts: Self::now_ms(),
            tool,
            args,
            // S-5 log hygiene: store the remote URL but never the credentials
            // embedded in it — the same userinfo strip the crash bundle's
            // redactor applies, so the rule has one implementation to audit.
            repo: crate::crash::redact_url_userinfo(&repo),
            sha,
            cwd_rel,
            gate,
            decision,
            reason,
            backend,
        }
    }

    /// Generate a unique run_id (short UUID-like string).
    ///
    /// Phase 1a: simple timestamp + random suffix. Phase 1.x may use proper UUIDs.
    fn generate_run_id() -> String {
        let ts = Self::now_ms();
        let rnd = (rand::random::<u32>() % 10000) as u64;
        format!("{:x}{:04x}", ts, rnd)
    }

    /// Get current Unix timestamp in milliseconds.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// Gate inputs: all GitGate checks captured for replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateInputs {
    /// True iff inside a git work tree.
    pub worktree: bool,
    /// True iff HEAD resolves (not unborn).
    pub head: bool,
    /// True iff configured remote exists.
    pub remote: bool,
    /// True iff working tree is clean (no untracked files).
    pub clean: bool,
}

/// Decision: where the run will execute.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Run on remote backend.
    Remote,
    /// Run locally (original decision).
    Local,
}

/// Deadline-expiry detail: which backend's watch ran out, on which of its
/// runs, and why (features.md v1.x "timeout/deadline config per backend").
///
/// Stamped onto the terminal record of a run whose remote watch outlived its
/// configured per-backend deadline — the expiry is classified upstream as
/// [`Verdict::InfraFailure`] (DD-4, never a fabricated verdict) and the run
/// degrades through the capped-local ladder, so this detail is how the
/// ledger's one terminal record still identifies the timeout instead of
/// losing it to the local rerun's outcome. The record's own `run_id` is the
/// gantry run identifier; `handle` is the backend's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutExpiry {
    /// The backend whose wait expired, in the config spelling
    /// ([`crate::cli::backend_name`]: "argo", "command", "none").
    pub backend: String,
    /// The backend's own run identifier — the abandoned run's handle (the
    /// argo workflow name, e.g.), still watchable on the remote after the
    /// expiry abandoned it.
    pub handle: String,
    /// The backend's expiry reason, verbatim — the same string the
    /// `[gantry] timeout` line printed.
    pub reason: String,
}

/// Verdict record: terminal record written on every exit path.
///
/// Completes the intent-verdict pair. Missing verdict = orphaned intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRecord {
    /// Record type discriminator ("verdict").
    #[serde(rename = "rec")]
    pub record_type: String,

    /// Schema version for backward compatibility.
    #[serde(rename = "schema_version")]
    pub schema_version: u32,

    /// Run identifier (must match the intent record).
    pub run_id: String,

    /// Unix timestamp (milliseconds) when verdict was recorded.
    pub ts: u64,

    /// Terminal verdict classification.
    pub verdict: Verdict,

    /// Where the run actually executed.
    pub ran: RanLocation,

    /// Exit code from the run.
    pub exit_code: i32,

    /// Failure taxonomy class (verdict.json v2, plan §Component 5): what the
    /// remote run died of — compile-error / test-failure / doctest /
    /// harness-panic — when it ran to completion, failed, and was
    /// instrumented with `--message-format json`. `None` for passes, infra,
    /// cancels, local runs, and uninstrumented producers; `gate-failure` is
    /// never derived, only attributed by the producer.
    ///
    /// Lenient on read, exactly like verdict.json: a class string a newer
    /// producer coined reads as absent rather than failing the record parse.
    /// Additive on write (skipped when absent), so records from either
    /// schema parse everywhere the ledger does — `SCHEMA_VERSION` stays 1.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::verdict::deserialize_lenient_failure_class"
    )]
    pub failure_class: Option<crate::verdict::FailureClass>,

    /// Deadline-expiry detail ([`TimeoutExpiry`]): present only on the
    /// terminal record of a run whose remote watch outlived its configured
    /// per-backend deadline. The expiry itself is classified InfraFailure
    /// (DD-4, never a fabricated verdict) and the run degrades through the
    /// capped-local ladder, so this is how the ledger's one terminal record
    /// still identifies the timeout — backend, run handle, expiry reason —
    /// instead of losing it to the local rerun's outcome. `None` on every
    /// other record. Additive on write (skipped when absent), so records
    /// from either schema parse everywhere the ledger does —
    /// `SCHEMA_VERSION` stays 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutExpiry>,

    /// Backend handle (workflow name, etc.) for `gantry why`.
    pub handle: String,

    /// Duration breakdown (milliseconds) for performance visibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durations_ms: Option<Durations>,
}

impl VerdictRecord {
    /// Create a new verdict record.
    ///
    /// Timestamp is generated automatically.
    pub fn new(
        run_id: String,
        verdict: Verdict,
        ran: RanLocation,
        exit_code: i32,
        handle: String,
        durations_ms: Option<Durations>,
    ) -> Self {
        VerdictRecord {
            record_type: "verdict".to_string(),
            schema_version: SCHEMA_VERSION,
            run_id,
            ts: Self::now_ms(),
            verdict,
            ran,
            exit_code,
            failure_class: None,
            timeout: None,
            handle,
            durations_ms,
        }
    }

    /// Stamp the failure class a remote run died of (verdict.json v2, plan
    /// §Component 5) onto the record.
    ///
    /// The remote arm's opt-in: `new()` defaults the field to `None` — the
    /// shape every local run, infra path, and uninstrumented producer writes
    /// per the field contract on the field above — and only the remote
    /// pipeline calls this, with the class its parsed document carried. The
    /// caller that knows the verdict decides whether a threaded class belongs
    /// on the record it arrived with ([`crate::decision`]); this builder only
    /// carries it.
    pub fn with_failure_class(
        mut self,
        failure_class: Option<crate::verdict::FailureClass>,
    ) -> Self {
        self.failure_class = failure_class;
        self
    }

    /// Stamp the deadline-expiry detail on the record.
    ///
    /// The expiry arm's opt-in: `new()` defaults the field to `None` — the
    /// shape every non-expiry record writes, per the field contract above —
    /// and only the deadline path calls this, with the backend, handle, and
    /// reason its expiry carried. The caller that watched the run decides
    /// whether an expiry belongs on the record it degrades ([`crate::decision`]);
    /// this builder only carries it.
    pub fn with_timeout(mut self, timeout: Option<TimeoutExpiry>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Get current Unix timestamp in milliseconds.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// Verdict: the terminal classification of a run.
///
/// Must match the backend::Verdict enum for consistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The remote ran the suite and it passed.
    Pass,
    /// The remote ran the suite and it failed.
    TestFailure,
    /// An enabled quality gate failed while tests passed.
    GateFailure,
    /// No verdict was produced - infra failure.
    InfraFailure,
    /// User canceled the run (Ctrl-C).
    Cancelled,
    /// A newer sha superseded this run.
    Superseded,
}

/// Human-readable verdict names for the `[gantry] verdict: …` trailer line.
/// Deliberately CamelCase — this is the transcript/`gantry why` spelling; the
/// serialized record keeps serde's snake_case (`"test_failure"`), and the two
/// are not meant to match.
impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Verdict::Pass => "Pass",
            Verdict::TestFailure => "TestFailure",
            Verdict::GateFailure => "GateFailure",
            Verdict::InfraFailure => "InfraFailure",
            Verdict::Cancelled => "Cancelled",
            Verdict::Superseded => "Superseded",
        };
        f.write_str(name)
    }
}

/// Where the run actually executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RanLocation {
    /// Ran on remote backend.
    Remote,
    /// Ran locally (original decision).
    Local,
    /// Ran locally after infra failure (fallback).
    LocalAfterInfra,
}

/// Duration breakdown (milliseconds) for performance visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Durations {
    /// Time spent in GitGate checks.
    pub gate: u64,
    /// Time spent pushing the epoch ref.
    pub push: u64,
    /// Time spent in remote queue.
    pub queue: u64,
    /// Time spent running the suite (remote or local).
    pub run: u64,
}

/// Orphaned run: intent with no matching verdict (lost run, INV-1).
///
/// Used by `gantry doctor` to report silently-skipped runs from SIGKILL,
/// crashes, or similar catastrophes.
#[derive(Debug, Clone)]
pub struct OrphanedRun {
    /// The run_id that has no matching verdict.
    pub run_id: String,
    /// The intent record that was never completed.
    pub intent: IntentRecord,
    /// When the orphan was detected (not necessarily when it happened).
    pub orphaned_at: SystemTime,
}

/// One logical run as read back from the ledger: the write-ahead OPEN intent
/// plus its terminal verdict when one landed. This is the unit `gantry why`
/// replays and `gantry status` lists.
#[derive(Debug, Clone)]
pub struct RunEntry {
    /// The OPEN intent written before dispatch (all gate inputs captured).
    pub intent: IntentRecord,
    /// The terminal verdict, `None` while the run is in flight — or forever,
    /// when the process was killed mid-run (an orphan doctor reports).
    pub verdict: Option<VerdictRecord>,
}

/// The result of reading the whole ledger ([`RunLog::read_entries`]).
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// Paired runs in file order (oldest first). File order is arrival
    /// order: runs.jsonl is append-only with O_APPEND single-line writes.
    pub entries: Vec<RunEntry>,
    /// Lines that could not be parsed and were skipped (torn final line,
    /// truncated record). Diagnostics answer from the records that parsed.
    pub skipped_lines: usize,
    /// Verdict records whose `run_id` matched no intent (the degenerate
    /// ineligible path writes one). Nothing to attach them to; counted so
    /// the number stays visible instead of silently dropped.
    pub unmatched_verdicts: usize,
}

/// Error type for RunLog operations.
#[derive(Debug)]
pub enum RunLogError {
    /// Cannot determine the state directory (HOME not set?).
    CannotDetermineStateDir,

    /// Cannot create the state directory.
    CannotCreateStateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    /// Cannot append to the log file.
    CannotAppend {
        path: PathBuf,
        source: std::io::Error,
    },

    /// Cannot read the log file.
    CannotReadLog {
        path: PathBuf,
        source: std::io::Error,
    },

    /// JSON serialization failed.
    Serialization {
        context: String,
        source: serde_json::Error,
    },

    /// Corrupt record in the log file.
    CorruptRecord {
        line_num: usize,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for RunLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunLogError::CannotDetermineStateDir => {
                write!(f, "cannot determine state directory (HOME not set)")
            }
            RunLogError::CannotCreateStateDir { path, source } => {
                write!(
                    f,
                    "cannot create state directory {}: {}",
                    path.display(),
                    source
                )
            }
            RunLogError::CannotAppend { path, source } => {
                write!(f, "cannot append to {}: {}", path.display(), source)
            }
            RunLogError::CannotReadLog { path, source } => {
                write!(f, "cannot read {}: {}", path.display(), source)
            }
            RunLogError::Serialization { context, source } => {
                write!(f, "failed to serialize {}: {}", context, source)
            }
            RunLogError::CorruptRecord { line_num, source } => {
                write!(f, "corrupt record at line {}: {}", line_num, source)
            }
        }
    }
}

impl std::error::Error for RunLogError {}

/// Simple random number generator for run_id generation.
mod rand {
    /// Simple XOR-shift random number generator.
    /// Phase 1a: good enough for run_id suffixes.
    pub fn random<T>() -> T
    where
        T: Default,
    {
        // In a real implementation, this would use a proper RNG.
        // For Phase 1a, we'll use system time as a fallback.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let val = now.wrapping_mul(1103515245).wrapping_add(12345);
        unsafe { std::mem::transmute_copy::<u64, T>(&val) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_record_serializes_correctly() {
        let intent = IntentRecord::new(
            "cargo".to_string(),
            vec![
                "test".to_string(),
                "--".to_string(),
                "--nocapture".to_string(),
            ],
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
            "clean".to_string(),
            "argo".to_string(),
        );

        let json = serde_json::to_string(&intent).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["rec"], "intent");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["run_id"].is_string());
        assert!(parsed["ts"].is_number());
        assert_eq!(parsed["tool"], "cargo");
        assert_eq!(parsed["args"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["decision"], "remote");
    }

    #[test]
    fn intent_repo_strips_embedded_credentials_s5() {
        // S-5 log hygiene: the runlog stores the remote URL as given but
        // never the credentials embedded in it — userinfo is stripped before
        // the intent is written.
        let intent = IntentRecord::new(
            "cargo".to_string(),
            vec!["test".to_string()],
            "https://ci:hunter2@git.example/repo.git".to_string(),
            "abc123".to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean: true,
            },
            Decision::Remote,
            "clean".to_string(),
            "argo".to_string(),
        );
        assert!(!intent.repo.contains("hunter2"), "{}", intent.repo);
        assert_eq!(intent.repo, "https://[REDACTED]@git.example/repo.git");

        // A clean URL is stored as given — S-5 strips userinfo, nothing else.
        let plain = IntentRecord::new(
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
            "clean".to_string(),
            "argo".to_string(),
        );
        assert_eq!(plain.repo, "https://github.com/example/repo");
    }

    #[test]
    fn verdict_record_serializes_correctly() {
        let verdict = VerdictRecord::new(
            "test-run-id".to_string(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "gantry-x7k2p".to_string(),
            Some(Durations {
                gate: 40,
                push: 900,
                queue: 12000,
                run: 341000,
            }),
        );

        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["rec"], "verdict");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["run_id"], "test-run-id");
        assert!(parsed["ts"].is_number());
        assert_eq!(parsed["verdict"], "pass");
        assert_eq!(parsed["ran"], "remote");
        assert_eq!(parsed["exit_code"], 0);
    }

    #[test]
    fn run_ids_are_unique() {
        let id1 = IntentRecord::generate_run_id();
        let id2 = IntentRecord::generate_run_id();

        assert_ne!(id1, id2);
    }

    #[test]
    fn orphan_detection_finds_unmatched_intents() {
        // This test requires a temporary log file
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("runs.jsonl");

        // Write an intent record
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
            "clean".to_string(),
            "argo".to_string(),
        );

        let intent_json = serde_json::to_string(&intent).unwrap();
        std::fs::write(&log_path, format!("{}\n", intent_json)).unwrap();

        // Create a RunLog pointing to the temp file
        let runlog = RunLog { log_path };

        // Find orphans
        let orphans = runlog.find_orphans().unwrap();

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run_id, intent.run_id);
    }

    #[test]
    fn orphan_detection_ignores_completed_runs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("runs.jsonl");

        // Write both intent and verdict
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
            "clean".to_string(),
            "argo".to_string(),
        );

        let verdict = VerdictRecord::new(
            intent.run_id.clone(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "gantry-x7k2p".to_string(),
            None,
        );

        let intent_json = serde_json::to_string(&intent).unwrap();
        let verdict_json = serde_json::to_string(&verdict).unwrap();

        std::fs::write(&log_path, format!("{}\n{}\n", intent_json, verdict_json)).unwrap();

        // Create a RunLog pointing to the temp file
        let runlog = RunLog { log_path };

        // Find orphans
        let orphans = runlog.find_orphans().unwrap();

        assert_eq!(orphans.len(), 0);
    }

    #[test]
    fn sigkill_mid_run_leaves_orphaned_intent() {
        // Acceptance test for Phase 1a: SIGKILL mid-run leaves an orphaned intent
        // that doctor reports (INV-1). This is the core write-ahead guarantee.

        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("runs.jsonl");

        // Create a RunLog instance
        let runlog = RunLog {
            log_path: log_path.clone(),
        };

        // Simulate a real run starting: write an intent record
        let intent = IntentRecord::new(
            "cargo".to_string(),
            vec![
                "test".to_string(),
                "--".to_string(),
                "--nocapture".to_string(),
            ],
            "https://github.com/example/repo".to_string(),
            "abc123def".to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean: true,
            },
            Decision::Remote,
            "clean".to_string(),
            "argo".to_string(),
        );

        let run_id = intent.run_id.clone();

        // Write the intent (this happens BEFORE dispatch in write-ahead logging)
        runlog.open_intent(&intent).unwrap();

        // Simulate SIGKILL mid-run: process dies before verdict is written
        // (In a real scenario, the process is killed and no verdict record is written)

        // Later, doctor queries for orphans
        let orphans = runlog.find_orphans().unwrap();

        // Verify that the orphaned intent is detected
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].run_id, run_id);
        assert_eq!(orphans[0].intent.tool, "cargo");
        assert_eq!(orphans[0].intent.decision, Decision::Remote);

        // The orphan should have all the original intent data for replay
        assert_eq!(orphans[0].intent.args.len(), 3);
        assert_eq!(orphans[0].intent.args[0], "test");
    }

    /// A reusable remote-run intent (the AS-2 shape: intercepted cargo test).
    fn test_intent() -> IntentRecord {
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
                clean: true,
            },
            Decision::Remote,
            String::new(),
            "command".to_string(),
        )
    }

    #[test]
    fn read_entries_pairs_intents_with_their_verdicts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };

        let intent = test_intent();
        let verdict = VerdictRecord::new(
            intent.run_id.clone(),
            Verdict::TestFailure,
            RanLocation::Remote,
            1,
            "gantry-x7k2p".to_string(),
            None,
        );
        runlog.open_intent(&intent).unwrap();
        runlog.close_verdict(&verdict).unwrap();

        let ledger = runlog.read_entries().unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.skipped_lines, 0);
        assert_eq!(ledger.unmatched_verdicts, 0);

        let entry = &ledger.entries[0];
        assert_eq!(entry.intent.run_id, intent.run_id);
        let verdict = entry.verdict.as_ref().expect("verdict must be paired");
        assert_eq!(verdict.verdict, Verdict::TestFailure);
        assert_eq!(verdict.ran, RanLocation::Remote);
    }

    #[test]
    fn read_entries_reports_unpaired_intents_as_verdictless() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };
        runlog.open_intent(&test_intent()).unwrap();

        let ledger = runlog.read_entries().unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert!(ledger.entries[0].verdict.is_none());
    }

    #[test]
    fn read_entries_skips_corrupt_lines_without_failing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("runs.jsonl");

        let intent = test_intent();
        let good = serde_json::to_string(&intent).unwrap();
        // A torn final line (crash mid-write) and a mid-file truncated record.
        let torn = format!("{}trunc", &good[..good.len() - 20]);
        std::fs::write(&log_path, format!("{{not json at all\n{torn}\n{good}\n\n")).unwrap();

        let runlog = RunLog { log_path };
        let ledger = runlog.read_entries().unwrap();

        assert_eq!(ledger.entries.len(), 1, "the good record must survive");
        assert_eq!(ledger.skipped_lines, 2, "both bad lines are counted");
    }

    #[test]
    fn read_entries_counts_verdicts_with_no_intent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };

        // The degenerate ineligible path writes a verdict with the stand-in
        // id "ineligible" and no intent record at all (decision.rs).
        let stray = VerdictRecord::new(
            "ineligible".to_string(),
            Verdict::InfraFailure,
            RanLocation::Local,
            1,
            "local".to_string(),
            None,
        );
        runlog.close_verdict(&stray).unwrap();

        let ledger = runlog.read_entries().unwrap();
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.unmatched_verdicts, 1);
    }

    #[test]
    fn read_entries_on_a_missing_ledger_is_an_empty_ledger() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("does-not-exist.jsonl"),
        };
        let ledger = runlog.read_entries().unwrap();
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.skipped_lines, 0);
    }

    #[test]
    fn read_entries_orders_runs_oldest_first() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };

        let first = test_intent();
        let second = test_intent();
        runlog.open_intent(&first).unwrap();
        runlog.open_intent(&second).unwrap();

        let ledger = runlog.read_entries().unwrap();
        assert_eq!(ledger.entries.len(), 2);
        assert_eq!(ledger.entries[0].intent.run_id, first.run_id);
        assert_eq!(ledger.entries[1].intent.run_id, second.run_id);
    }

    /// Every FailureClass survives the record's serde round trip as its
    /// kebab-case wire name (plan §Component 5): the class a producer
    /// stamped reads back as the same class, not as absent.
    #[test]
    fn verdict_record_failure_class_round_trips_through_the_kebab_wire_form() {
        let classes = [
            (crate::verdict::FailureClass::CompileError, "compile-error"),
            (crate::verdict::FailureClass::TestFailure, "test-failure"),
            (crate::verdict::FailureClass::Doctest, "doctest"),
            (crate::verdict::FailureClass::HarnessPanic, "harness-panic"),
            (crate::verdict::FailureClass::GateFailure, "gate-failure"),
        ];
        for (class, wire) in &classes {
            let verdict = VerdictRecord::new(
                "class-run".to_string(),
                Verdict::TestFailure,
                RanLocation::Remote,
                101,
                "gantry-x7k2p".to_string(),
                None,
            )
            .with_failure_class(Some(class.clone()));

            let json = serde_json::to_string(&verdict).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed["failure_class"], *wire, "class {class:?}");

            let round: VerdictRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(round.failure_class.as_ref(), Some(class), "class {class:?}");
            assert_eq!(round.run_id, "class-run");
            assert_eq!(round.verdict, Verdict::TestFailure);
            assert_eq!(round.ran, RanLocation::Remote);
            assert_eq!(round.exit_code, 101);
        }
    }

    /// A None class — the `new()` default every local run, infra path, and
    /// uninstrumented producer writes — is additive on write: the key is
    /// skipped entirely, and an old schema-1 line without the key reads
    /// back as None rather than demanding it.
    #[test]
    fn verdict_record_without_failure_class_skips_the_field_and_reads_back_none() {
        let verdict = VerdictRecord::new(
            "plain-run".to_string(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "gantry-x7k2p".to_string(),
            None,
        );
        assert_eq!(verdict.failure_class, None);

        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.get("failure_class").is_none(),
            "an absent class must not be written: {json}"
        );

        let round: VerdictRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(round.failure_class, None);

        // The pre-taxonomy producer shape: no failure_class key at all.
        let schema1: VerdictRecord = serde_json::from_str(
            r#"{"rec":"verdict","schema_version":1,"run_id":"old","ts":1,"verdict":"pass","ran":"remote","exit_code":0,"handle":"gantry-x7k2p"}"#,
        )
        .unwrap();
        assert_eq!(schema1.failure_class, None);
    }

    /// A class string a newer producer coined reads as absent, not a parse
    /// failure — the same leniency contract verdict.json gives the field,
    /// shared through one deserializer. The core signals survive the read.
    #[test]
    fn verdict_record_unknown_failure_class_reads_as_absent_not_an_error() {
        let line = r#"{"rec":"verdict","schema_version":1,"run_id":"future","ts":2,"verdict":"test_failure","ran":"remote","exit_code":101,"failure_class":"lockfile-drift","handle":"gantry-x7k2p"}"#;
        let record: VerdictRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.failure_class, None);
        assert_eq!(record.run_id, "future");
        assert_eq!(record.verdict, Verdict::TestFailure);
        assert_eq!(record.exit_code, 101);
    }

    /// End to end through the ledger: only the instrumented remote failure
    /// carries its class into runs.jsonl — the kebab string lands on exactly
    /// that entry's line — while pass, infra, cancel, and local-fallback
    /// exits keep the field off their lines, so reading the ledger back
    /// never misattributes a class to them.
    #[test]
    fn runs_jsonl_carries_the_class_only_on_the_class_bearing_remote_failure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };

        // (verdict, ran, exit_code, threaded class) per run. The first is
        // the instrumented remote failure; the rest are the no-class exits.
        let outcomes = [
            (
                Verdict::TestFailure,
                RanLocation::Remote,
                101,
                Some(crate::verdict::FailureClass::TestFailure),
            ),
            (Verdict::Pass, RanLocation::Remote, 0, None),
            (Verdict::InfraFailure, RanLocation::Remote, 1, None),
            (Verdict::Cancelled, RanLocation::Remote, 130, None),
            (
                Verdict::TestFailure,
                RanLocation::LocalAfterInfra,
                101,
                None,
            ),
        ];

        let mut intents = Vec::with_capacity(outcomes.len());
        for _ in 0..outcomes.len() {
            let mut intent = test_intent();
            while intents
                .iter()
                .any(|prior: &IntentRecord| prior.run_id == intent.run_id)
            {
                intent = test_intent();
            }
            intents.push(intent);
        }

        for ((verdict, ran, exit_code, class), intent) in outcomes.iter().zip(&intents) {
            let record = VerdictRecord::new(
                intent.run_id.clone(),
                *verdict,
                *ran,
                *exit_code,
                "gantry-x7k2p".to_string(),
                None,
            )
            .with_failure_class(class.clone());
            runlog.open_intent(intent).unwrap();
            runlog.close_verdict(&record).unwrap();
        }

        // Exactly one line in the raw ledger names a class, and it is the
        // remote failure's kebab string.
        let raw = std::fs::read_to_string(&runlog.log_path).unwrap();
        let class_lines: Vec<&str> = raw
            .lines()
            .filter(|l| l.contains("failure_class"))
            .collect();
        assert_eq!(
            class_lines.len(),
            1,
            "only the class-bearing entry may name a class: {raw}"
        );
        assert!(
            class_lines[0].contains(r#""failure_class":"test-failure""#),
            "{}",
            class_lines[0]
        );

        // Reading the ledger back: the class rides the remote failure;
        // every other exit reads as None.
        let ledger = runlog.read_entries().unwrap();
        assert_eq!(ledger.entries.len(), outcomes.len());
        for (entry, (_, _, _, expected)) in ledger.entries.iter().zip(outcomes.iter()) {
            let record = entry.verdict.as_ref().expect("verdict must be paired");
            assert_eq!(
                record.failure_class, *expected,
                "run {}",
                entry.intent.run_id
            );
        }
    }

    /// The deadline-expiry detail survives the record's serde round trip
    /// with backend, handle, and reason intact (features.md v1.x
    /// "timeout/deadline config per backend"): the expiry the deadline path
    /// stamped reads back as the same expiry, not as absent, and the
    /// degraded record's own signals ride along. The additive field does
    /// not bump the schema (the field contract above): the record still
    /// declares `SCHEMA_VERSION`, and the wire line still reads
    /// schema_version 1.
    #[test]
    fn verdict_record_timeout_round_trips_backend_handle_and_reason() {
        let expiry = TimeoutExpiry {
            backend: "argo".to_string(),
            handle: "gantry-x7k2p".to_string(),
            reason: "workflow gantry-x7k2p deadline exceeded while polling status.phase"
                .to_string(),
        };
        let verdict = VerdictRecord::new(
            "timeout-run".to_string(),
            Verdict::TestFailure,
            RanLocation::LocalAfterInfra,
            101,
            "gantry-x7k2p".to_string(),
            None,
        )
        .with_timeout(Some(expiry.clone()));

        assert_eq!(verdict.schema_version, SCHEMA_VERSION);

        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["timeout"]["backend"], "argo");
        assert_eq!(parsed["timeout"]["handle"], "gantry-x7k2p");
        assert_eq!(
            parsed["timeout"]["reason"],
            "workflow gantry-x7k2p deadline exceeded while polling status.phase"
        );

        let round: VerdictRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(round.timeout.as_ref(), Some(&expiry));
        assert_eq!(round.schema_version, SCHEMA_VERSION);
        assert_eq!(round.run_id, "timeout-run");
        assert_eq!(round.verdict, Verdict::TestFailure);
        assert_eq!(round.ran, RanLocation::LocalAfterInfra);
        assert_eq!(round.exit_code, 101);
    }

    /// A None expiry — the `new()` default every non-expiry record writes —
    /// is additive on write: the key is skipped entirely, so non-expiry
    /// bytes are unchanged, and a runs.jsonl line from a producer that
    /// never knew the field reads back as None rather than demanding it.
    #[test]
    fn verdict_record_without_timeout_skips_the_field_and_reads_back_none() {
        let verdict = VerdictRecord::new(
            "plain-run".to_string(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "gantry-x7k2p".to_string(),
            None,
        );
        assert_eq!(verdict.timeout, None);

        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.get("timeout").is_none(),
            "an absent expiry must not be written: {json}"
        );

        let round: VerdictRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(round.timeout, None);

        // The pre-expiry producer shape: no timeout key at all.
        let schema1: VerdictRecord = serde_json::from_str(
            r#"{"rec":"verdict","schema_version":1,"run_id":"old","ts":1,"verdict":"pass","ran":"remote","exit_code":0,"handle":"gantry-x7k2p"}"#,
        )
        .unwrap();
        assert_eq!(schema1.timeout, None);
        assert_eq!(schema1.schema_version, 1);
    }

    /// End to end through the ledger: only the deadline-expired run carries
    /// its expiry into runs.jsonl — backend, handle, and reason land on
    /// exactly that entry's terminal line — while every other exit keeps
    /// the field off its lines, so reading the ledger back never
    /// misattributes an expiry to them.
    #[test]
    fn runs_jsonl_carries_the_expiry_only_on_the_timeout_bearing_record() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runlog = RunLog {
            log_path: temp_dir.path().join("runs.jsonl"),
        };

        // (verdict, ran, exit_code, threaded expiry) per run. The first is
        // the deadline-expired run's terminal record, closed by the
        // capped-local ladder with the local rerun's outcome; the rest are
        // the no-expiry exits.
        let expiry = TimeoutExpiry {
            backend: "argo".to_string(),
            handle: "gantry-x7k2p".to_string(),
            reason: "workflow gantry-x7k2p deadline exceeded while polling status.phase"
                .to_string(),
        };
        let outcomes = [
            (
                Verdict::TestFailure,
                RanLocation::LocalAfterInfra,
                101,
                Some(expiry),
            ),
            (Verdict::Pass, RanLocation::Remote, 0, None),
            (Verdict::InfraFailure, RanLocation::Remote, 1, None),
            (Verdict::TestFailure, RanLocation::Remote, 101, None),
        ];

        let mut intents = Vec::with_capacity(outcomes.len());
        for _ in 0..outcomes.len() {
            let mut intent = test_intent();
            while intents
                .iter()
                .any(|prior: &IntentRecord| prior.run_id == intent.run_id)
            {
                intent = test_intent();
            }
            intents.push(intent);
        }

        for ((verdict, ran, exit_code, timeout), intent) in outcomes.iter().zip(&intents) {
            let record = VerdictRecord::new(
                intent.run_id.clone(),
                *verdict,
                *ran,
                *exit_code,
                "gantry-x7k2p".to_string(),
                None,
            )
            .with_timeout(timeout.clone());
            runlog.open_intent(intent).unwrap();
            runlog.close_verdict(&record).unwrap();
        }

        // Exactly one line in the raw ledger names an expiry, and it is
        // the degraded run's backend and handle in the wire form.
        let raw = std::fs::read_to_string(&runlog.log_path).unwrap();
        let timeout_lines: Vec<&str> = raw
            .lines()
            .filter(|l| l.contains(r#""timeout":"#))
            .collect();
        assert_eq!(
            timeout_lines.len(),
            1,
            "only the expiry-bearing entry may name a timeout: {raw}"
        );
        assert!(
            timeout_lines[0].contains(r#""timeout":{"backend":"argo","handle":"gantry-x7k2p""#),
            "{}",
            timeout_lines[0]
        );

        // Reading the ledger back: the expiry rides the degraded run;
        // every other exit reads as None.
        let ledger = runlog.read_entries().unwrap();
        assert_eq!(ledger.entries.len(), outcomes.len());
        for (entry, (_, _, _, expected)) in ledger.entries.iter().zip(outcomes.iter()) {
            let record = entry.verdict.as_ref().expect("verdict must be paired");
            assert_eq!(record.timeout, *expected, "run {}", entry.intent.run_id);
        }
    }
}
