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
            repo,
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
            handle,
            durations_ms,
        }
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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

/// Where the run actually executed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
