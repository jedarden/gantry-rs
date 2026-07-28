// gantry — RefPusher epoch-ref push (plan §"Phase 1a", Components §4 "RefPusher").
//
// Phase 1a implementation (bf-3v5): lease-based GC, opportunistic sweep, branch mode.
// RefPusher::push runs `git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>`,
// where epoch is a monotonic unix-second prefix. Each push creates a client-side
// lease record and opportunistically sweeps expired+terminal refs only (EC-09).
// The legacy `push_mode = "branch"` escape hatch is supported with a warning.
// See docs/notes/epoch-refs-design.md for the full design rationale.

use crate::config::{Config, PushMode};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// Constants
// ============================================================================

/// Default lease duration in seconds (1 hour).
const DEFAULT_LEASE_DURATION_SECONDS: u64 = 3600;

/// Default retention window in days (7 days).
const DEFAULT_RETENTION_DAYS: u64 = 7;

/// Current schema version for lease records.
pub const LEASE_SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Data models
// ============================================================================

/// Result of a RefPusher push operation.
///
/// The happy path is `success=true`; any failure yields `success=false` with a
/// reason string. Push failures map to InfraFailure in the decision engine.
#[derive(Debug, Clone, PartialEq)]
pub struct PushResult {
    /// True iff the push succeeded.
    pub success: bool,
    /// Human-readable reason when unsuccessful; empty string when successful.
    pub reason: String,
    /// The ref name that was pushed (or attempted).
    pub ref_name: String,
}

/// Client-side lease record (EC-09: local lease records, no cross-box coordination).
///
/// Each push creates a lease record that protects the ref while a run is in-flight.
/// Leases are stored in `~/.local/state/gantry/leases.jsonl` and are used for
/// opportunistic GC sweep (plan §4 "RefPusher").
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LeaseRecord {
    /// Schema version for migration handling.
    pub schema_version: u32,
    /// The ref name (e.g., "refs/gantry/1720441234-abc123...").
    pub ref_name: String,
    /// The run ID (links to RunLog intent record).
    pub run_id: String,
    /// When this lease was created (unix seconds).
    pub created_at: u64,
    /// When this lease expires (unix seconds).
    pub expires_at: u64,
    /// Whether the run is terminal (completed).
    pub terminal: bool,
}

impl LeaseRecord {
    /// Create a new lease record for a run.
    fn new(ref_name: String, run_id: String, lease_duration_secs: u64) -> Self {
        let now = Self::now_seconds();
        LeaseRecord {
            schema_version: LEASE_SCHEMA_VERSION,
            ref_name,
            run_id,
            created_at: now,
            expires_at: now + lease_duration_secs,
            terminal: false,
        }
    }

    /// Check if this lease has expired.
    fn is_expired(&self) -> bool {
        Self::now_seconds() > self.expires_at
    }

    /// Get current time as unix seconds.
    fn now_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is before unix epoch")
            .as_secs()
    }
}

/// Ref state from ls-remote for sweep decisions.
struct RemoteRef {
    /// The ref name.
    name: String,
    /// The epoch extracted from the ref name.
    epoch: u64,
}

impl PushResult {
    /// The happy-path result: push succeeded.
    fn success(ref_name: String) -> Self {
        PushResult {
            success: true,
            reason: String::new(),
            ref_name,
        }
    }

    /// A failed push: unsuccessful with a reason.
    fn failed(reason: &str, ref_name: String) -> Self {
        PushResult {
            success: false,
            reason: reason.to_string(),
            ref_name,
        }
    }
}

/// RefPusher: ship a commit to the remote as a hidden content-addressed ref.
///
/// Phase 1a implementation (bf-3v5):
/// - Supports both ref mode (`refs/gantry/<epoch>-<sha>`) and legacy branch mode.
/// - Creates client-side lease records for GC sweep (EC-09).
/// - Opportunistically sweeps expired+terminal refs after each successful push.
/// - Maps push failures to InfraFailure (via `success=false`).
///
/// Branch mode issues a warning about mutation risk (S-1, INV-2) but is
/// explicitly allowed as a legacy escape hatch.
pub struct RefPusher;

impl RefPusher {
    /// Push a commit to the remote as an epoch ref (or branch in legacy mode).
    ///
    /// Phase 1a (bf-3v5) behavior:
    /// - Ref mode: `git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>`
    /// - Branch mode: `git push <ci_remote> <sha>:refs/heads/gantry/<sha>` with warning
    /// - Creates client-side lease record for GC sweep
    /// - Opportunistically sweeps expired+terminal refs after successful push
    /// - Maps push failures to `success=false` (InfraFailure in decision engine)
    ///
    /// ## Parameters
    ///
    /// - `config`: The Config (provides `ci_remote`, `push_mode`, backend settings).
    /// - `sha`: The commit SHA to push (typically HEAD).
    /// - `run_id`: The run ID for lease record linking.
    ///
    /// ## Returns
    ///
    /// `PushResult` with `success=true` iff the push succeeded; otherwise
    /// `success=false` with a human-readable reason. The `ref_name` field
    /// contains the ref that was (or would have been) created.
    pub fn push(config: &Config, sha: &str, run_id: &str) -> PushResult {
        // Generate the epoch prefix: unix seconds (monotonic, time-derived).
        // This is the GC lever (plan §4 "RefPusher").
        let epoch = Self::epoch_now();

        // Build the ref name based on push_mode
        // Ref mode: refs/gantry/<epoch>-<sha> (default, recommended)
        // Branch mode: refs/heads/gantry/<sha> (legacy, warns about mutation risk)
        let ref_name = match &config.remote.push_mode {
            PushMode::Ref => {
                format!("refs/gantry/{epoch}-{sha}")
            }
            PushMode::Branch => {
                // Emit warning about mutation risk (S-1, INV-2)
                eprintln!(
                    "[gantry] warning: push_mode=branch creates a mutable branch ref (refs/heads/gantry/{sha}).",
                );
                eprintln!("[gantry] warning: this violates the 'no branch mutation' invariant and is a legacy escape hatch.");
                eprintln!("[gantry] warning: prefer push_mode=ref (hidden refs under refs/gantry/*) for production use.");
                format!("refs/heads/gantry/{sha}")
            }
        };

        // Run the push: git push <ci_remote> <sha>:<ref_name>
        match Self::run_git_push(&config.remote.ci_remote, sha, &ref_name) {
            Ok(_) => {
                // Push succeeded: create lease record
                let lease = LeaseRecord::new(
                    ref_name.clone(),
                    run_id.to_string(),
                    DEFAULT_LEASE_DURATION_SECONDS,
                );
                if let Err(e) = Self::create_lease(&lease) {
                    // Non-fatal: log warning but continue
                    eprintln!("[gantry] warning: failed to create lease record: {e}");
                }

                // Opportunistic sweep of expired+terminal refs (EC-09)
                if let Err(e) = Self::opportunistic_sweep(config) {
                    // Non-fatal: log warning but continue
                    eprintln!("[gantry] warning: GC sweep failed: {e}");
                }

                PushResult::success(ref_name)
            }
            Err(e) => PushResult::failed(&e, ref_name),
        }
    }

    /// Mark a lease as terminal (run completed).
    ///
    /// Called when a run finishes (pass, fail, gate_failure, etc.). The ref
    /// is kept for the retention window but marked as no longer in-flight.
    pub fn mark_terminal(run_id: &str) -> Result<(), String> {
        let leases_path = Self::leases_path()?;
        let file =
            File::open(&leases_path).map_err(|e| format!("failed to open leases file: {e}"))?;

        let reader = BufReader::new(file);
        let mut updated_leases = Vec::new();
        let mut found = false;

        for line in reader.lines() {
            let line = line.map_err(|e| format!("failed to read lease line: {e}"))?;
            if let Ok(mut lease) = serde_json::from_str::<LeaseRecord>(&line) {
                if lease.run_id == run_id && !lease.terminal {
                    lease.terminal = true;
                    found = true;
                }
                updated_leases.push(lease);
            }
        }

        if !found {
            return Ok(()); // No lease found or already terminal
        }

        // Rewrite the leases file with updated records
        Self::rewrite_leases(&updated_leases)?;
        Ok(())
    }

    /// Create a lease record in the state directory.
    fn create_lease(lease: &LeaseRecord) -> Result<(), String> {
        let leases_path = Self::leases_path()?;

        // Ensure state directory exists
        if let Some(parent) = leases_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create leases directory: {e}"))?;
        }

        // Append the lease record to the JSONL file
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&leases_path)
            .map_err(|e| format!("failed to open leases file: {e}"))?;

        let json =
            serde_json::to_string(lease).map_err(|e| format!("failed to serialize lease: {e}"))?;

        writeln!(file, "{json}").map_err(|e| format!("failed to write lease: {e}"))?;
        Ok(())
    }

    /// Get the path to the leases.jsonl file.
    fn leases_path() -> Result<PathBuf, String> {
        let state_dir = dirs::state_dir()
            .ok_or_else(|| "cannot determine state directory".to_string())?
            .join("gantry");

        Ok(state_dir.join("leases.jsonl"))
    }

    /// Rewrite the leases file with updated records.
    fn rewrite_leases(leases: &[LeaseRecord]) -> Result<(), String> {
        let leases_path = Self::leases_path()?;
        let temp_path = leases_path.with_extension("tmp");

        let mut file = File::create(&temp_path)
            .map_err(|e| format!("failed to create temp leases file: {e}"))?;

        for lease in leases {
            let json = serde_json::to_string(lease)
                .map_err(|e| format!("failed to serialize lease: {e}"))?;
            writeln!(file, "{json}").map_err(|e| format!("failed to write lease: {e}"))?;
        }

        // Atomic rename
        fs::rename(&temp_path, &leases_path)
            .map_err(|e| format!("failed to rename leases file: {e}"))?;

        Ok(())
    }

    /// Opportunistic GC sweep of expired+terminal refs (EC-09).
    ///
    /// Sweeps only refs created by this box (tracked via local lease records).
    /// Each box GCs independently; no cross-box coordination needed.
    ///
    /// ## Sweep logic
    ///
    /// 1. List all `refs/gantry/*` refs on the remote via `ls-remote`.
    /// 2. For each ref, check if we have a local lease record.
    /// 3. Delete iff: `epoch < sweep_threshold` AND lease expired.
    ///
    /// Refs whose epoch is recent (>= sweep_threshold) are never touched,
    /// regardless of lease state. This is the safety guarantee (plan §4).
    fn opportunistic_sweep(config: &Config) -> Result<(), String> {
        let sweep_threshold = Self::epoch_now() - (DEFAULT_RETENTION_DAYS * 24 * 3600);

        // Load all local leases
        let local_leases = Self::load_local_leases()?;

        // List all refs on the remote
        let remote_refs = Self::list_remote_refs(&config.remote.ci_remote)?;

        // Find refs to delete: epoch < threshold AND lease expired
        let refs_to_delete: Vec<String> = remote_refs
            .into_iter()
            .filter_map(|r| {
                // Check if we have a local lease for this ref
                local_leases
                    .iter()
                    .find(|l| l.ref_name == r.name)
                    .and_then(|lease| {
                        if r.epoch < sweep_threshold && lease.is_expired() {
                            Some(r.name.clone())
                        } else {
                            None
                        }
                    })
            })
            .collect();

        if refs_to_delete.is_empty() {
            return Ok(());
        }

        // Delete the refs in a single git push call (with :ref syntax)
        for ref_name in refs_to_delete {
            match Self::delete_remote_ref(&config.remote.ci_remote, &ref_name) {
                Ok(_) => {
                    eprintln!("[gantry] gc: deleted expired ref {ref_name}");
                }
                Err(e) => {
                    eprintln!("[gantry] gc: failed to delete {ref_name}: {e}");
                }
            }
        }

        Ok(())
    }

    /// Load all local lease records.
    pub fn load_local_leases() -> Result<Vec<LeaseRecord>, String> {
        let leases_path = Self::leases_path()?;

        if !leases_path.exists() {
            return Ok(Vec::new());
        }

        let file =
            File::open(&leases_path).map_err(|e| format!("failed to open leases file: {e}"))?;

        let reader = BufReader::new(file);
        let mut leases = Vec::new();

        for line in reader.lines() {
            let line = line.map_err(|e| format!("failed to read lease line: {e}"))?;
            if let Ok(lease) = serde_json::from_str::<LeaseRecord>(&line) {
                leases.push(lease);
            }
        }

        Ok(leases)
    }

    /// List all `refs/gantry/*` refs on the remote via `ls-remote`.
    fn list_remote_refs(ci_remote: &str) -> Result<Vec<RemoteRef>, String> {
        let output = Command::new("git")
            .args(["ls-remote", ci_remote, "refs/gantry/*"])
            .output()
            .map_err(|e| format!("git ls-remote failed: {e}"))?;

        if !output.status.success() {
            // No refs found is not an error - return empty list
            if String::from_utf8_lossy(&output.stderr).contains("found") {
                return Ok(Vec::new());
            }
            return Err(format!(
                "git ls-remote failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut refs = Vec::new();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let ref_name = parts[1].to_string();
                if let Some(epoch) = Self::extract_epoch_from_ref(&ref_name) {
                    refs.push(RemoteRef {
                        name: ref_name,
                        epoch,
                    });
                }
            }
        }

        Ok(refs)
    }

    /// Extract epoch from a ref name (e.g., "refs/gantry/1720441234-abc..." -> 1720441234).
    fn extract_epoch_from_ref(ref_name: &str) -> Option<u64> {
        // Extract epoch from refs/gantry/<epoch>-<sha>
        let pattern = "refs/gantry/";
        if !ref_name.starts_with(pattern) {
            return None;
        }

        let rest = &ref_name[pattern.len()..];
        let dash_idx = rest.find('-')?;
        let epoch_str = &rest[..dash_idx];
        epoch_str.parse::<u64>().ok()
    }

    /// Delete a remote ref via `git push <remote> :<ref_name>`.
    fn delete_remote_ref(ci_remote: &str, ref_name: &str) -> Result<(), String> {
        let output = Command::new("git")
            .args(["push", ci_remote, &format!(":{ref_name}")])
            .output()
            .map_err(|e| format!("git push delete failed: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git push delete failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }

    /// Get the current epoch as unix seconds.
    fn epoch_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is before unix epoch")
            .as_secs()
    }

    /// Run `git push <ci_remote> <sha>:<ref_name>`.
    ///
    /// Shells out to system git (no libgit2 — matches predecessor behavior and
    /// GitGate). Returns Ok(()) iff the push succeeded; otherwise returns an
    /// Err with a human-readable reason string.
    ///
    /// ## Parameters
    ///
    /// - `ci_remote`: The remote name (e.g., "origin").
    /// - `sha`: The commit SHA to push.
    /// - `ref_name`: The fully-qualified ref name (e.g., "refs/gantry/123-abc").
    ///
    /// ## Returns
    ///
    /// Ok(()) if the push succeeded, Err(reason) otherwise.
    fn run_git_push(ci_remote: &str, sha: &str, ref_name: &str) -> Result<(), String> {
        let output = Command::new("git")
            .args(["push", ci_remote, &format!("{sha}:{ref_name}")])
            .output()
            .map_err(|e| format!("git push command failed: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git push failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }

    /// Verify that no branch refs were moved on the remote (INV-2).
    ///
    /// This is called by tests to assert invariant INV-2: "no branch ref ever moves".
    /// Compares ls-remote output before and after a push to ensure no refs/heads/*
    /// refs were created or modified.
    #[cfg(test)]
    pub fn assert_no_branch_refs_moved(
        _ci_remote: &str,
        before_output: &str,
        after_output: &str,
    ) -> Result<(), String> {
        // Extract refs/heads/* refs from before and after
        let before_branches: Vec<String> = before_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1].starts_with("refs/heads/") {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .collect();

        let after_branches: Vec<String> = after_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1].starts_with("refs/heads/") {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .collect();

        // Check that no new branch refs were created
        let new_branches: Vec<_> = after_branches
            .iter()
            .filter(|b| !before_branches.contains(b))
            .collect();

        if !new_branches.is_empty() {
            return Err(format!(
                "INV-2 violated: new branch refs created: {:?}",
                new_branches
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::Mutex;

    // Serialize tests that change the current directory to avoid interference
    static DIR_MUTEX: Mutex<()> = Mutex::new(());

    // --- filesystem fixtures ------------------------------------------------

    /// A throwaway directory under the system temp, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(suffix: &str) -> Self {
            let tid = std::thread::current()
                .name()
                .unwrap_or("unknown")
                .replace(":", "_");
            let random = std::process::id() as u64
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
            let dir = std::env::temp_dir().join(format!(
                "gantry-test-{}-{}-{}-{suffix}",
                std::process::id(),
                tid,
                random
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run a git command in a directory and return the output.
    fn git_cmd(dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .map_err(|e| format!("git command failed: {e}"))?;

        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Create a minimal git repo with one commit and an 'origin' remote.
    ///
    /// Returns (repo_dir, remote_dir, sha) where sha is the commit hash.
    fn setup_repo_with_remote() -> (TempDir, TempDir, String) {
        let repo_dir = TempDir::new("repo");
        let remote_dir = TempDir::new("bare-remote");

        // Initialize bare remote
        let remote_path = remote_dir.path().to_str().expect("remote path is utf-8");
        let output = Command::new("git")
            .args(["init", "--bare", remote_path])
            .output()
            .expect("git init bare remote failed");

        if !output.status.success() {
            panic!(
                "git init --bare failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Initialize the main repo
        git_cmd(repo_dir.path(), &["init"]).expect("git init");
        git_cmd(repo_dir.path(), &["config", "user.name", "Test User"])
            .expect("git config user.name");
        git_cmd(
            repo_dir.path(),
            &["config", "user.email", "test@example.com"],
        )
        .expect("git config user.email");

        // Add origin remote pointing at the bare repo
        let remote_url = remote_dir.path().to_str().expect("remote path is utf-8");
        git_cmd(repo_dir.path(), &["remote", "add", "origin", remote_url]).expect("git remote add");

        // Create a commit so HEAD resolves
        let test_file = repo_dir.path().join("test.txt");
        fs::write(&test_file, "test content").expect("write test file");
        git_cmd(repo_dir.path(), &["add", "."]).expect("git add");
        git_cmd(repo_dir.path(), &["commit", "-m", "Initial commit"]).expect("git commit");

        // Get the commit SHA
        let sha = git_cmd(repo_dir.path(), &["rev-parse", "HEAD"]).expect("git rev-parse HEAD");

        (repo_dir, remote_dir, sha)
    }

    #[test]
    fn push_creates_epoch_ref_on_remote() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let config = Config::hardcoded();
        let run_id = "test-run-123";
        let result = RefPusher::push(&config, &sha, run_id);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            result.success,
            "push should succeed, got reason: {}",
            result.reason
        );
        assert!(
            result.reason.is_empty(),
            "push should have empty reason, got: {}",
            result.reason
        );

        // Verify the ref was created on the remote
        let output = Command::new("git")
            .args(["ls-remote", remote_dir.path().to_str().unwrap()])
            .output()
            .expect("git ls-remote failed");

        assert!(output.status.success(), "ls-remote should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&sha),
            "ls-remote output should contain the sha {sha}, got: {stdout}"
        );
        assert!(
            stdout.contains("refs/gantry/"),
            "ls-remote output should contain refs/gantry/, got: {stdout}"
        );
    }

    #[test]
    fn push_does_not_modify_branch_refs() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Get ls-remote before push
        let before = Command::new("git")
            .args(["ls-remote", remote_dir.path().to_str().unwrap()])
            .output()
            .expect("git ls-remote before failed");

        assert!(before.status.success(), "ls-remote before should succeed");

        // Run the push
        let config = Config::hardcoded();
        let run_id = "test-run-inv2";
        let result = RefPusher::push(&config, &sha, run_id);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(result.success, "push should succeed");

        // Get ls-remote after push
        let after = Command::new("git")
            .args(["ls-remote", remote_dir.path().to_str().unwrap()])
            .output()
            .expect("git ls-remote after failed");

        assert!(after.status.success(), "ls-remote after should succeed");

        // Verify no refs/heads/* refs exist (no branch refs were created) - INV-2
        let before_stdout = String::from_utf8_lossy(&before.stdout);
        let after_stdout = String::from_utf8_lossy(&after.stdout);

        assert!(
            !before_stdout.contains("refs/heads/"),
            "ls-remote before should not contain refs/heads/, got: {before_stdout}"
        );
        assert!(
            !after_stdout.contains("refs/heads/"),
            "ls-remote after should not contain refs/heads/, got: {after_stdout}"
        );
    }

    #[test]
    fn push_creates_lease_record() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let config = Config::hardcoded();
        let run_id = "test-run-lease-123";
        let result = RefPusher::push(&config, &sha, run_id);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(result.success, "push should succeed");

        // Verify lease record was created
        let leases = RefPusher::load_local_leases().expect("failed to load leases");
        let lease = leases.iter().find(|l| l.run_id == run_id);

        assert!(lease.is_some(), "lease record should be created");
        let lease = lease.unwrap();
        assert_eq!(lease.run_id, run_id);
        assert!(!lease.terminal, "new lease should not be terminal");
        assert!(lease.ref_name.contains("refs/gantry/"));
    }

    #[test]
    fn push_to_nonexistent_remote_fails() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Create a config with a nonexistent remote
        let mut config = Config::hardcoded();
        config.remote.ci_remote = "nonexistent-remote".to_string();

        let run_id = "test-run-fail-remote";
        let result = RefPusher::push(&config, &sha, run_id);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(!result.success, "push to nonexistent remote should fail");
        assert!(
            !result.reason.is_empty(),
            "push failure should have a reason"
        );
    }

    #[test]
    fn push_result_success_returns_true_with_empty_reason() {
        let ref_name = "refs/gantry/123-abc".to_string();
        let success = PushResult::success(ref_name);
        assert!(success.success);
        assert!(success.reason.is_empty());
        assert_eq!(success.ref_name, "refs/gantry/123-abc");
    }

    #[test]
    fn push_result_failed_returns_false_with_reason() {
        let reason = "test reason";
        let ref_name = "refs/gantry/123-abc".to_string();
        let failed = PushResult::failed(reason, ref_name);
        assert!(!failed.success);
        assert_eq!(failed.reason, reason);
        assert_eq!(failed.ref_name, "refs/gantry/123-abc");
    }

    #[test]
    fn epoch_now_returns_positive_unix_seconds() {
        let epoch = RefPusher::epoch_now();
        assert!(epoch > 0, "epoch should be positive");
        // Sanity check: epoch should be roughly current time (2026-07-28 is ~1720440000)
        assert!(epoch > 1_720_440_000, "epoch should be >= 2026-07-28");
    }

    #[test]
    fn extract_epoch_from_ref_works() {
        let ref_name = "refs/gantry/1720441234-abc123def456";
        let epoch = RefPusher::extract_epoch_from_ref(ref_name);
        assert_eq!(epoch, Some(1720441234));

        let bad_ref = "refs/heads/main";
        let epoch = RefPusher::extract_epoch_from_ref(bad_ref);
        assert_eq!(epoch, None);
    }

    #[test]
    fn lease_record_expiry_check_works() {
        let now = LeaseRecord::now_seconds();
        let mut lease = LeaseRecord::new(
            "refs/gantry/123-abc".to_string(),
            "test-run".to_string(),
            100,
        );

        // Fresh lease should not be expired
        assert!(!lease.is_expired());

        // Old lease should be expired
        lease.expires_at = now - 1000;
        assert!(lease.is_expired());
    }

    #[test]
    fn mark_terminal_marks_lease_as_terminal() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let config = Config::hardcoded();
        let run_id = "test-run-terminal-123";
        let result = RefPusher::push(&config, &sha, run_id);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(result.success, "push should succeed");

        // Mark as terminal
        RefPusher::mark_terminal(run_id).expect("mark_terminal should succeed");

        // Verify lease is marked terminal
        let leases = RefPusher::load_local_leases().expect("failed to load leases");
        let lease = leases.iter().find(|l| l.run_id == run_id);

        assert!(lease.is_some(), "lease record should exist");
        assert!(lease.unwrap().terminal, "lease should be marked terminal");
    }
}
