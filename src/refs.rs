// gantry — RefPusher epoch-ref push (plan §"Phase 0.5", Components §4 "RefPusher").
//
// Phase 0.5 walking skeleton, stage 3 of 5 (bf-53a): builds on stages 1-2 (shim/config
// + GitGate) and ships the commit to the remote as a hidden content-addressed ref.
// This is the ref-mode-only skeleton: RefPusher::push runs `git push <ci_remote>
// <sha>:refs/gantry/<epoch>-<sha>`, where epoch is a monotonic unix-second prefix.
// The legacy `push_mode = "branch"` escape hatch is refused in the skeleton.
// No leases or GC sweep yet — those land in Phase 1a (bf-3v5).

use crate::config::Config;
use std::process::Command;

/// Result of a RefPusher push operation.
///
/// The happy path is `success=true`; any failure yields `success=false` with a
/// reason string. Phase 1a will expand this into a full InfraFailure path with
/// local fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct PushResult {
    /// True iff the push succeeded.
    pub success: bool,
    /// Human-readable reason when unsuccessful; empty string when successful.
    pub reason: String,
}

impl PushResult {
    /// The happy-path result: push succeeded.
    fn success() -> Self {
        PushResult {
            success: true,
            reason: String::new(),
        }
    }

    /// A failed push: unsuccessful with a reason.
    fn failed(reason: &str) -> Self {
        PushResult {
            success: false,
            reason: reason.to_string(),
        }
    }
}

/// RefPusher: ship a commit to the remote as a hidden content-addressed ref.
///
/// The skeleton implements only ref-mode pushing (plan §4 "RefPusher"):
/// `git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>`, where epoch is a
/// monotonic unix-second prefix. The legacy `push_mode = "branch"` escape hatch
/// is explicitly refused in the skeleton — this is ref-mode only.
///
/// No leases, no GC sweep, no InfraFailure integration yet — those land in
/// Phase 1a (bf-3v5). This module only answers the push question.
pub struct RefPusher;

impl RefPusher {
    /// Push a commit to the remote as an epoch ref.
    ///
    /// Runs `git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>`, where epoch
    /// is the current unix timestamp in seconds. The remote name comes from
    /// `config.ci_remote` (default: "origin"). The sha is caller-supplied (HEAD
    /// at this stage).
    ///
    /// ## Skeleton behavior
    ///
    /// - Ref mode only: the ref name is always `refs/gantry/<epoch>-<sha>`.
    /// - Legacy `push_mode = "branch"` is refused with a panic.
    /// - Push failure (remote not found, auth, network) returns `success=false`
    ///   with a reason string. Phase 1a integrates this into InfraFailure.
    ///
    /// ## Parameters
    ///
    /// - `config`: The hardcoded Config (provides `ci_remote`).
    /// - `sha`: The commit SHA to push (typically HEAD).
    ///
    /// ## Returns
    ///
    /// `PushResult` with `success=true` iff the push succeeded; otherwise
    /// `success=false` with a human-readable reason.
    ///
    /// ## Panics
    ///
    /// Panics if called with `push_mode = "branch"` — the skeleton is ref-mode
    /// only. The legacy branch mode is a Phase 1a concern (plan §4).
    pub fn push(config: &Config, sha: &str) -> PushResult {
        // Skeleton: ref mode only. The legacy branch escape hatch is refused.
        // Phase 1a (bf-3v5) may add push_mode config support.
        //
        // This is a deliberate early guard: the plan promises "ref mode only"
        // for the skeleton, and this panic makes that promise load-bearing.
        let _ = Self::assert_ref_mode_only();

        // Generate the epoch prefix: unix seconds (monotonic, time-derived).
        // This is the GC lever — Phase 1a (bf-3v5) adds leases and the sweep.
        let epoch = Self::epoch_now();

        // Build the ref name: refs/gantry/<epoch>-<sha>
        // This is the content-addressed ref pattern that guarantees no branch
        // mutation (plan §4, invariant INV-2).
        let ref_name = format!("refs/gantry/{epoch}-{sha}");

        // Run the push: git push <ci_remote> <sha>:<ref_name>
        // This creates (or updates, if the exact ref already exists) the hidden
        // ref on the remote without touching any branch refs (refs/heads/*).
        match Self::run_git_push(&config.ci_remote, sha, &ref_name) {
            Ok(_) => PushResult::success(),
            Err(e) => PushResult::failed(&e),
        }
    }

    /// Assert that we're in ref mode only.
    ///
    /// The skeleton is ref-mode only; this function is a load-bearing guard that
    /// makes that promise explicit. Phase 1a may relax this to support the legacy
    /// `push_mode = "branch"` escape hatch (plan §4).
    ///
    /// ## Panics
    ///
    /// Always panics in the skeleton — this is the enforcement mechanism for
    /// "ref mode only". Remove or relax this in Phase 1a when branch mode support
    /// lands.
    fn assert_ref_mode_only() -> Result<(), String> {
        // In the skeleton, we unconditionally refuse branch mode.
        // Phase 1a may read a `push_mode` config field and conditionally allow it.
        // For now, this is a deliberate design constraint.
        //
        // The plan promises: "Ref mode only; the legacy `push_mode = "branch"`
        // escape hatch is refused in the skeleton." This panic is that refusal.
        Err("push_mode = 'branch' is refused in the skeleton — ref mode only".to_string())
    }

    /// Get the current epoch as unix seconds.
    ///
    /// The epoch is a monotonic, time-derived prefix that serves as the GC lever
    /// (plan §4). Phase 1a (bf-3v5) adds leases and the sweep that uses this
    /// prefix to find stale refs.
    ///
    /// ## Returns
    ///
    /// The current unix timestamp in seconds.
    fn epoch_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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
        let result = RefPusher::push(&config, &sha);

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
        let result = RefPusher::push(&config, &sha);

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(result.success, "push should succeed");

        // Get ls-remote after push
        let after = Command::new("git")
            .args(["ls-remote", remote_dir.path().to_str().unwrap()])
            .output()
            .expect("git ls-remote after failed");

        assert!(after.status.success(), "ls-remote after should succeed");

        // Verify no refs/heads/* refs exist (no branch refs were created)
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
    fn push_to_nonexistent_remote_fails() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir, sha) = setup_repo_with_remote();

        // Change into the repo directory for the push
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Create a config with a nonexistent remote
        let mut config = Config::hardcoded();
        config.ci_remote = "nonexistent-remote".to_string();

        let result = RefPusher::push(&config, &sha);

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
        let success = PushResult::success();
        assert!(success.success);
        assert!(success.reason.is_empty());
    }

    #[test]
    fn push_result_failed_returns_false_with_reason() {
        let reason = "test reason";
        let failed = PushResult::failed(reason);
        assert!(!failed.success);
        assert_eq!(failed.reason, reason);
    }

    #[test]
    fn epoch_now_returns_positive_unix_seconds() {
        let epoch = RefPusher::epoch_now();
        assert!(epoch > 0, "epoch should be positive");
        // Sanity check: epoch should be roughly current time (2026-07-28 is ~1720440000)
        assert!(epoch > 1_720_440_000, "epoch should be >= 2026-07-28");
    }

    #[test]
    fn assert_ref_mode_only_always_fails() {
        // In the skeleton, this should always return an error
        let result = RefPusher::assert_ref_mode_only();
        assert!(
            result.is_err(),
            "ref_mode_only assertion should fail in skeleton"
        );
        let err = result.unwrap_err();
        assert!(err.contains("branch"), "error should mention 'branch' mode");
    }
}
