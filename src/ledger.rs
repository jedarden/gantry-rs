// gantry — ledger intelligence over runs.jsonl (plan Component 10).
//
// One verdict-history module powering three features, all reading the same
// append-only ledger `RunLog` already writes (no second state file):
//
// - **Memoization (opt-in, never default):** an identical
//   `(tree-hash, args, toolchain, image digest)` with a terminal PASS verdict
//   returns the recorded verdict instantly with a `[gantry] cached` line and
//   a `--fresh` escape (`GANTRY_FRESH=1`). Keyed on tree hash, not commit
//   sha, so rebases/amends with identical trees still hit. Failures are
//   never memoized.
// - **Supersede-on-new-commit:** a newer sha submitted for the same
//   `(repo, args)` marks the older still-running sibling with an explicit
//   `superseded` terminal record — recorded, never silent. Supersede never
//   yanks a watched run: it applies only when the older run has zero live
//   attachments (originator process gone; JoinTable waiters join this check
//   when that component lands). A sibling whose liveness cannot be proven
//   dead is never yanked.
// - **Flake flagging:** the same tree + args with flipping verdicts marks
//   runs `flaky-suspect` in runs.jsonl and the stderr summary.
//
// ## Ledger-key fields are additive
//
// The key parts ride `IntentRecord`/`VerdictRecord` as optional fields
// (`tree_hash`, `pid`, `toolchain`, `image_digest`, `flaky_suspect`) with
// `skip_serializing_if` — the same additive convention `failure_class` set:
// `SCHEMA_VERSION` stays 1, records from either shape parse everywhere the
// ledger does, and a record missing a key part simply never matches a memo
// key, never gets superseded, and never flags a flake.
//
// ## A cache hit is not a run
//
// A memo hit appends no ledger records: the PASS record that earned the
// verdict stays the only evidence, and `gantry status`/`doctor` orphan
// accounting keeps meaning "a dispatch actually happened".

use crate::runlog::{
    Decision, IntentRecord, RanLocation, RunEntry, RunLog, Verdict, VerdictRecord,
};

/// True when memoization is opted in via `GANTRY_MEMOIZE` (`1`/`true`).
///
/// Opt-in, never default (plan Component 10): a memoized rerun deliberately
/// does not execute the suite, so the caller has to ask for it.
pub fn memoize_requested() -> bool {
    memoize_requested_with(std::env::var("GANTRY_MEMOIZE").ok().as_deref())
}

/// [`memoize_requested`], decided from an explicit env value — the same
/// hermetic-test seam `state::check_enabled_with` uses (tests never touch the
/// process-global environment).
pub fn memoize_requested_with(env: Option<&str>) -> bool {
    matches!(env, Some("1") | Some("true"))
}

/// True when the `--fresh` escape is requested via `GANTRY_FRESH`
/// (`1`/`true`): forces a real execution even with memoization on.
pub fn fresh_requested() -> bool {
    fresh_requested_with(std::env::var("GANTRY_FRESH").ok().as_deref())
}

/// [`fresh_requested`], decided from an explicit env value.
pub fn fresh_requested_with(env: Option<&str>) -> bool {
    matches!(env, Some("1") | Some("true"))
}

/// The backend image digest for the memo key, from `GANTRY_IMAGE_DIGEST`.
///
/// The command backend's submit template decides what actually executes, so
/// gantry cannot see an image digest by itself; a producer that pins one sets
/// this env var and it becomes part of the memo key. Unset means "no digest
/// constraint" and only matches records recorded the same way.
pub fn current_image_digest() -> Option<String> {
    std::env::var("GANTRY_IMAGE_DIGEST")
        .ok()
        .filter(|d| !d.is_empty())
}

/// The toolchain identity for the memo key: `rustc --version` output.
///
/// `None` when rustc is absent or fails — a run whose toolchain cannot be
/// established must not serve a cached verdict (conservative: unknown key →
/// no hit).
pub fn rustc_toolchain() -> Option<String> {
    let output = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// The memoization key: `(tree-hash, args, toolchain, image digest)`.
///
/// Tree hash, not commit sha — a rebase or amend that lands identical content
/// still hits (plan Component 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoKey {
    /// `git rev-parse HEAD^{tree}` at intent time.
    pub tree_hash: String,
    /// The intercepted tool (Phase 0.5: always `cargo`).
    pub tool: String,
    /// The intercepted arguments, verbatim.
    pub args: Vec<String>,
    /// `rustc --version` output.
    pub toolchain: String,
    /// `GANTRY_IMAGE_DIGEST` when the producer pins one.
    pub image_digest: Option<String>,
}

impl MemoKey {
    /// Assemble a key from captured parts; `None` when a mandatory part
    /// (tree hash or toolchain) is missing — an incomplete key never serves
    /// a cached verdict.
    pub fn from_parts(
        tree_hash: Option<String>,
        toolchain: Option<String>,
        image_digest: Option<String>,
        tool: &str,
        args: &[String],
    ) -> Option<Self> {
        Some(MemoKey {
            tree_hash: tree_hash?,
            toolchain: toolchain?,
            image_digest,
            tool: tool.to_string(),
            args: args.to_vec(),
        })
    }

    /// Capture the key for the caller's current tree and toolchain.
    pub fn capture(tool: &str, args: &[String]) -> Option<Self> {
        Self::from_parts(
            crate::gate::head_tree_hash().ok(),
            rustc_toolchain(),
            current_image_digest(),
            tool,
            args,
        )
    }
}

/// Look up a memoized PASS verdict for `key` in the ledger.
///
/// Returns the newest matching terminal PASS run, or `None`. Records missing
/// any key part (older schema shapes) never match, and neither do failures —
/// memoization is PASS-only (plan Component 10).
pub fn lookup_cached_pass(log: &RunLog, key: &MemoKey) -> Option<RunEntry> {
    let ledger = log.read_entries().ok()?;
    // File order is arrival order (oldest first), so the newest hit is the
    // last match — scan newest-first and take the first match.
    ledger.entries.into_iter().rev().find(|entry| {
        matches!(
            entry.verdict.as_ref().map(|v| v.verdict),
            Some(Verdict::Pass)
        ) && key_matches(&entry.intent, key)
    })
}

/// Whether an intent record carries exactly this memo key.
fn key_matches(intent: &IntentRecord, key: &MemoKey) -> bool {
    intent.tree_hash.as_deref() == Some(key.tree_hash.as_str())
        && intent.tool == key.tool
        && intent.args == key.args
        && intent.toolchain.as_deref() == Some(key.toolchain.as_str())
        && intent.image_digest == key.image_digest
}

/// Supersede-on-new-commit (plan Component 10).
///
/// Scans the ledger for still-running siblings of `fresh` — OPEN intents with
/// the same `(repo, tool, args)` and a *different* sha — and appends an
/// explicit `superseded` terminal record for each one whose originator
/// process is gone (zero live attachments). The record's `handle` names the
/// run that displaced it (`superseded-by:<run-id>`), so `gantry why` keeps
/// the story readable.
///
/// Never yanks: same-sha siblings run alongside (JoinTable dedup is that
/// component's job), a sibling whose intent predates the `pid` field has
/// unprovable liveness, and a live originator means the run is watched.
/// Local-decision intents are out of scope — they finish inside their own
/// process, and an OPEN local intent with a dead pid is exactly the orphan
/// `gantry doctor` must keep reporting as a lost run.
///
/// Returns the run ids marked superseded (for tests and callers that want to
/// react); the stderr lines are printed here so every supersede is loud.
pub fn supersede_stale_siblings(log: &RunLog, fresh: &IntentRecord) -> Vec<String> {
    let ledger = match log.read_entries() {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };

    let mut superseded = Vec::new();
    for entry in &ledger.entries {
        let sibling = &entry.intent;
        if sibling.run_id == fresh.run_id || entry.verdict.is_some() {
            continue; // not us, and only still-running runs
        }
        if sibling.decision != Decision::Remote {
            continue; // local intents belong to doctor's orphan accounting
        }
        if sibling.repo != fresh.repo || sibling.tool != fresh.tool || sibling.args != fresh.args {
            continue; // a different (repo, args) family
        }
        if sibling.sha == fresh.sha {
            continue; // same sha runs alongside — only a *newer* sha supersedes
        }
        let unattached = match sibling.pid {
            Some(pid) => !pid_alive(pid),
            None => false, // liveness unprovable → never yank
        };
        if !unattached {
            continue;
        }

        let record = VerdictRecord::new(
            sibling.run_id.clone(),
            Verdict::Superseded,
            RanLocation::Remote,
            0,
            format!("superseded-by:{}", fresh.run_id),
            None,
        );
        if log.close_verdict(&record).is_ok() {
            eprintln!(
                "[gantry] superseded: run {} (sha {}) — replaced by newer commit {}",
                sibling.run_id,
                short_sha(&sibling.sha),
                short_sha(&fresh.sha),
            );
            superseded.push(sibling.run_id.clone());
        }
    }
    superseded
}

/// Flake flagging (plan Component 10).
///
/// Called before a terminal record is appended: when the ledger already holds
/// a terminal *test outcome* (Pass / TestFailure) for the same
/// `(tree_hash, tool, args)` with the opposite outcome, the record is marked
/// `flaky-suspect` and the stderr summary names both runs. Returns whether
/// the record was flagged.
///
/// Only Pass↔TestFailure flips count: `GateFailure` is deterministic given a
/// tree (a flip is a tooling problem, not a flaky suite), and infra/cancel/
/// superseded verdicts say nothing about the suite. Records without a tree
/// hash can never be compared.
pub fn mark_flaky_if_flip(log: &RunLog, intent: &IntentRecord, record: &mut VerdictRecord) -> bool {
    let Some(tree) = intent.tree_hash.as_deref() else {
        return false;
    };
    let Some(current) = test_outcome(record.verdict) else {
        return false;
    };
    let ledger = match log.read_entries() {
        Ok(l) => l,
        Err(_) => return false,
    };

    for entry in &ledger.entries {
        let Some(prior) = entry.verdict.as_ref() else {
            continue; // still running
        };
        let Some(previous) = test_outcome(prior.verdict) else {
            continue; // not a test outcome
        };
        if previous == current || entry.intent.run_id == record.run_id {
            continue;
        }
        let same_tree = entry.intent.tree_hash.as_deref() == Some(tree);
        let same_command = entry.intent.tool == intent.tool && entry.intent.args == intent.args;
        if same_tree && same_command {
            record.flaky_suspect = Some(true);
            eprintln!(
                "[gantry] flaky-suspect: {} flips {} at tree {} (prior run {}) — same tree+args, opposite outcomes",
                record.verdict,
                prior.verdict,
                short_sha(tree),
                entry.intent.run_id,
            );
            return true;
        }
    }
    false
}

/// The boolean test outcome a verdict carries, `None` for verdicts that say
/// nothing about the suite.
fn test_outcome(verdict: Verdict) -> Option<bool> {
    match verdict {
        Verdict::Pass => Some(true),
        Verdict::TestFailure => Some(false),
        Verdict::GateFailure | Verdict::InfraFailure | Verdict::Cancelled | Verdict::Superseded => {
            None
        }
    }
}

/// Whether an originator process is still alive.
///
/// Linux reads `/proc/<pid>`; elsewhere the conservative answer is "alive" —
/// supersede never yanks a run it cannot prove is unattached. PID reuse can
/// only ever make a dead originator look alive, which is the safe direction.
fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

/// First 8 characters of a sha for stderr lines (hex input, so 8 bytes is a
/// char boundary; still guarded against shorter inputs).
pub fn short_sha(sha: &str) -> &str {
    match sha.char_indices().nth(8) {
        Some((idx, _)) => &sha[..idx],
        None => sha,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runlog::GateInputs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A RunLog over a throwaway state dir — the hermetic seam
    /// (`RunLog::at_path`), so tests never touch the real `$HOME` ledger.
    fn temp_log() -> (TempDir, RunLog) {
        let dir = TempDir::new().expect("tempdir");
        let log = RunLog::at_path(dir.path().join("runs.jsonl"));
        (dir, log)
    }

    /// An intent with the ledger-key fields applied.
    struct IntentSpec {
        run_id: &'static str,
        sha: &'static str,
        pid: Option<u32>,
        tree_hash: Option<&'static str>,
        toolchain: Option<&'static str>,
        image_digest: Option<&'static str>,
        decision: Decision,
        tool: &'static str,
        args: Vec<String>,
    }

    fn intent(spec: IntentSpec) -> IntentRecord {
        let mut intent = IntentRecord::new(
            spec.tool.to_string(),
            spec.args,
            "https://example.com/repo.git".to_string(),
            spec.sha.to_string(),
            PathBuf::from("."),
            GateInputs {
                worktree: true,
                head: true,
                remote: true,
                clean: true,
            },
            spec.decision,
            "clean".to_string(),
            "command".to_string(),
        );
        intent.run_id = spec.run_id.to_string();
        intent.pid = spec.pid;
        intent.tree_hash = spec.tree_hash.map(str::to_string);
        intent.toolchain = spec.toolchain.map(str::to_string);
        intent.image_digest = spec.image_digest.map(str::to_string);
        intent
    }

    fn spec(run_id: &'static str, sha: &'static str, tree: &'static str) -> IntentSpec {
        IntentSpec {
            run_id,
            sha,
            pid: None,
            tree_hash: Some(tree),
            toolchain: Some("rustc 1.98.1 (797e8a9bc 2026-08-05)"),
            image_digest: None,
            decision: Decision::Remote,
            tool: "cargo",
            args: vec!["test".to_string()],
        }
    }

    fn key_for(tree: &str, toolchain: &str, image: Option<&str>, args: &[&str]) -> MemoKey {
        MemoKey {
            tree_hash: tree.to_string(),
            tool: "cargo".to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            toolchain: toolchain.to_string(),
            image_digest: image.map(str::to_string),
        }
    }

    fn write_pass(log: &RunLog, intent: &IntentRecord) {
        log.open_intent(intent).expect("open_intent");
        log.close_verdict(&VerdictRecord::new(
            intent.run_id.clone(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            "wf-1".to_string(),
            None,
        ))
        .expect("close_verdict");
    }

    #[test]
    fn env_gating_accepts_only_the_on_values() {
        assert!(!memoize_requested_with(None));
        assert!(!memoize_requested_with(Some("0")));
        assert!(memoize_requested_with(Some("1")));
        assert!(memoize_requested_with(Some("true")));
        assert!(!fresh_requested_with(None));
        assert!(fresh_requested_with(Some("1")));
        assert!(fresh_requested_with(Some("true")));
    }

    #[test]
    fn memo_hit_requires_every_key_part_to_match() {
        let (_dir, log) = temp_log();
        write_pass(&log, &intent(spec("run-base", "aaaa1111", "tree1111")));
        let toolchain = "rustc 1.98.1 (797e8a9bc 2026-08-05)";

        // Exact key → hit.
        assert!(
            lookup_cached_pass(&log, &key_for("tree1111", toolchain, None, &["test"])).is_some()
        );
        // Different tree (rebases with identical trees hit; different trees do not).
        assert!(
            lookup_cached_pass(&log, &key_for("tree2222", toolchain, None, &["test"])).is_none()
        );
        // Different toolchain.
        assert!(
            lookup_cached_pass(&log, &key_for("tree1111", "rustc 1.99.0", None, &["test"]))
                .is_none()
        );
        // Different args.
        assert!(lookup_cached_pass(
            &log,
            &key_for("tree1111", toolchain, None, &["test", "--lib"])
        )
        .is_none());
        // Image digest mismatch in either direction.
        assert!(lookup_cached_pass(
            &log,
            &key_for("tree1111", toolchain, Some("sha256:abc"), &["test"])
        )
        .is_none());
    }

    #[test]
    fn memo_never_serves_failures_or_keyless_records() {
        let (_dir, log) = temp_log();

        // A terminal TestFailure is never memoized.
        let failed = intent(spec("run-fail", "aaaa1111", "tree1111"));
        log.open_intent(&failed).expect("open_intent");
        log.close_verdict(&VerdictRecord::new(
            failed.run_id.clone(),
            Verdict::TestFailure,
            RanLocation::Remote,
            1,
            "wf-2".to_string(),
            None,
        ))
        .expect("close_verdict");
        assert!(lookup_cached_pass(
            &log,
            &key_for(
                "tree1111",
                "rustc 1.98.1 (797e8a9bc 2026-08-05)",
                None,
                &["test"]
            )
        )
        .is_none());

        // A PASS recorded before the ledger-key fields existed (no
        // tree_hash) can never match — it carries no provable key.
        let mut legacy = spec("run-legacy", "aaaa1111", "tree1111");
        legacy.tree_hash = None;
        let legacy = intent(legacy);
        write_pass(&log, &legacy);
        assert!(lookup_cached_pass(
            &log,
            &key_for(
                "tree1111",
                "rustc 1.98.1 (797e8a9bc 2026-08-05)",
                None,
                &["test"]
            )
        )
        .is_none());
    }

    #[test]
    fn memo_serves_the_newest_matching_pass() {
        let (_dir, log) = temp_log();
        write_pass(&log, &intent(spec("run-old", "aaaa1111", "tree1111")));
        write_pass(&log, &intent(spec("run-new", "bbbb2222", "tree1111")));
        let hit = lookup_cached_pass(
            &log,
            &key_for(
                "tree1111",
                "rustc 1.98.1 (797e8a9bc 2026-08-05)",
                None,
                &["test"],
            ),
        )
        .expect("hit");
        assert_eq!(hit.intent.run_id, "run-new");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn supersede_marks_only_dead_pid_remote_siblings() {
        let (_dir, log) = temp_log();

        // A dead pid: a child that has been waited on.
        let mut dead = std::process::Command::new("true").spawn().expect("spawn");
        dead.wait().expect("wait");

        let old = intent(IntentSpec {
            pid: Some(dead.id()),
            ..spec("run-old", "aaaa1111", "tree1111")
        });
        log.open_intent(&old).expect("open_intent");

        // Live originator (this test process) → runs alongside.
        let watched = intent(IntentSpec {
            run_id: "run-watched",
            pid: Some(std::process::id()),
            ..spec("run-watched", "bbbb2222", "tree1111")
        });
        log.open_intent(&watched).expect("open_intent");

        // No pid recorded → liveness unprovable → never yanked.
        let keyless = spec("run-keyless", "cccc3333", "tree1111");
        log.open_intent(&intent(keyless)).expect("open_intent");

        // Same sha as the fresh run → runs alongside.
        let same_sha = intent(IntentSpec {
            run_id: "run-same-sha",
            pid: Some(dead.id()),
            ..spec("run-same-sha", "eeee5555", "tree1111")
        });
        log.open_intent(&same_sha).expect("open_intent");

        let fresh = intent(spec("run-fresh", "eeee5555", "tree9999"));
        log.open_intent(&fresh).expect("open_intent");

        let superseded = supersede_stale_siblings(&log, &fresh);
        assert_eq!(superseded, vec!["run-old".to_string()]);

        let ledger = log.read_entries().expect("read_entries");
        let verdict_of = |id: &str| {
            ledger
                .entries
                .iter()
                .find(|e| e.intent.run_id == id)
                .and_then(|e| e.verdict.as_ref().map(|v| v.verdict))
        };
        assert_eq!(verdict_of("run-old"), Some(Verdict::Superseded));
        assert_eq!(verdict_of("run-watched"), None);
        assert_eq!(verdict_of("run-keyless"), None);
        assert_eq!(verdict_of("run-same-sha"), None);
        assert_eq!(verdict_of("run-fresh"), None);

        // The superseded record names its displacer.
        let entry = ledger
            .entries
            .iter()
            .find(|e| e.intent.run_id == "run-old")
            .expect("run-old entry");
        assert_eq!(
            entry.verdict.as_ref().expect("verdict").handle,
            "superseded-by:run-fresh"
        );
    }

    #[test]
    fn supersede_skips_terminal_local_and_mismatched_runs() {
        let (_dir, log) = temp_log();

        // Already terminal → out of scope.
        write_pass(&log, &intent(spec("run-done", "aaaa1111", "tree1111")));
        // Local-decision intent → doctor's orphan accounting, not ours.
        let local = intent(IntentSpec {
            run_id: "run-local",
            decision: Decision::Local,
            ..spec("run-local", "bbbb2222", "tree1111")
        });
        log.open_intent(&local).expect("open_intent");
        // Different args → a different (repo, args) family.
        let other_args = intent(IntentSpec {
            run_id: "run-other-args",
            args: vec!["build".to_string()],
            ..spec("run-other-args", "cccc3333", "tree1111")
        });
        log.open_intent(&other_args).expect("open_intent");
        // Different repo → out of scope even with identical args.
        let mut other_repo = spec("run-other-repo", "dddd4444", "tree1111");
        other_repo.args = vec!["test".to_string()];
        let mut other_repo = intent(other_repo);
        other_repo.repo = "https://example.com/other.git".to_string();
        log.open_intent(&other_repo).expect("open_intent");

        let fresh = intent(spec("run-fresh", "eeee5555", "tree9999"));
        log.open_intent(&fresh).expect("open_intent");

        assert!(supersede_stale_siblings(&log, &fresh).is_empty());
        let ledger = log.read_entries().expect("read_entries");
        // run-done keeps its terminal verdict (that is exactly why supersede
        // skips it); the OPEN siblings stay open.
        for id in ["run-local", "run-other-args", "run-other-repo"] {
            assert!(
                ledger
                    .entries
                    .iter()
                    .find(|e| e.intent.run_id == id)
                    .expect("entry")
                    .verdict
                    .is_none(),
                "{id} must stay open"
            );
        }
    }

    #[test]
    fn flake_flag_lands_on_the_flipping_record_only() {
        let (_dir, log) = temp_log();
        write_pass(&log, &intent(spec("run-pass", "aaaa1111", "tree1111")));

        // A TestFailure at the same tree+args flips → flagged.
        let failing = intent(spec("run-fail", "bbbb2222", "tree1111"));
        log.open_intent(&failing).expect("open_intent");
        let mut record = VerdictRecord::new(
            failing.run_id.clone(),
            Verdict::TestFailure,
            RanLocation::Remote,
            1,
            "wf-3".to_string(),
            None,
        );
        assert!(mark_flaky_if_flip(&log, &failing, &mut record));
        assert_eq!(record.flaky_suspect, Some(true));

        // The appended record carries the flag through serialization.
        log.close_verdict(&record).expect("close_verdict");
        let ledger = log.read_entries().expect("read_entries");
        let stored = ledger
            .entries
            .iter()
            .find(|e| e.intent.run_id == "run-fail")
            .expect("entry")
            .verdict
            .as_ref()
            .expect("verdict");
        assert_eq!(stored.flaky_suspect, Some(true));

        // The original PASS record stays unflagged (the ledger is
        // append-only; only the flipping run is marked).
        let first = ledger
            .entries
            .iter()
            .find(|e| e.intent.run_id == "run-pass")
            .expect("entry")
            .verdict
            .as_ref()
            .expect("verdict");
        assert_eq!(first.flaky_suspect, None);
    }

    #[test]
    fn flake_flag_requires_same_tree_args_and_opposite_outcomes() {
        let (_dir, log) = temp_log();
        write_pass(&log, &intent(spec("run-pass", "aaaa1111", "tree1111")));

        // Same outcome at the same tree → no flip.
        let again = intent(spec("run-again", "bbbb2222", "tree1111"));
        log.open_intent(&again).expect("open_intent");
        let mut record = VerdictRecord::new(
            again.run_id.clone(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            String::new(),
            None,
        );
        assert!(!mark_flaky_if_flip(&log, &again, &mut record));
        assert_eq!(record.flaky_suspect, None);

        // Opposite outcome at a different tree → not a flake.
        let elsewhere = intent(spec("run-elsewhere", "cccc3333", "tree9999"));
        log.open_intent(&elsewhere).expect("open_intent");
        let mut record = VerdictRecord::new(
            elsewhere.run_id.clone(),
            Verdict::TestFailure,
            RanLocation::Remote,
            1,
            String::new(),
            None,
        );
        assert!(!mark_flaky_if_flip(&log, &elsewhere, &mut record));
    }

    #[test]
    fn flake_flag_ignores_non_outcomes_and_keyless_records() {
        let (_dir, log) = temp_log();

        // Infra between outcomes is not a flip: pass, infra, pass.
        write_pass(&log, &intent(spec("run-pass", "aaaa1111", "tree1111")));
        let infra = intent(spec("run-infra", "bbbb2222", "tree1111"));
        log.open_intent(&infra).expect("open_intent");
        let mut record = VerdictRecord::new(
            infra.run_id.clone(),
            Verdict::InfraFailure,
            RanLocation::Remote,
            1,
            String::new(),
            None,
        );
        assert!(!mark_flaky_if_flip(&log, &infra, &mut record));
        log.close_verdict(&record).expect("close_verdict");
        let later_pass = intent(spec("run-later", "cccc3333", "tree1111"));
        log.open_intent(&later_pass).expect("open_intent");
        let mut record = VerdictRecord::new(
            later_pass.run_id.clone(),
            Verdict::Pass,
            RanLocation::Remote,
            0,
            String::new(),
            None,
        );
        assert!(!mark_flaky_if_flip(&log, &later_pass, &mut record));

        // A flipping record without a tree hash can never be compared.
        let mut keyless = spec("run-keyless-fail", "dddd4444", "tree1111");
        keyless.tree_hash = None;
        let keyless = intent(keyless);
        log.open_intent(&keyless).expect("open_intent");
        let mut record = VerdictRecord::new(
            keyless.run_id.clone(),
            Verdict::TestFailure,
            RanLocation::Remote,
            1,
            String::new(),
            None,
        );
        assert!(!mark_flaky_if_flip(&log, &keyless, &mut record));
    }

    #[test]
    fn memo_key_requires_tree_and_toolchain() {
        assert!(
            MemoKey::from_parts(None, Some("rustc 1.98.1".to_string()), None, "cargo", &[])
                .is_none()
        );
        assert!(MemoKey::from_parts(Some("tree".into()), None, None, "cargo", &[]).is_none());
        assert!(MemoKey::from_parts(
            Some("tree".into()),
            Some("rustc 1.98.1".into()),
            Some("sha256:abc".into()),
            "cargo",
            &["test".to_string()]
        )
        .is_some());
    }

    #[test]
    fn short_sha_is_guarded_against_short_input() {
        assert_eq!(short_sha("0123456789abcdef"), "01234567");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }
}
