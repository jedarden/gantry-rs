// gantry — GitGate eligibility checks (plan §"Phase 0.5", Components §3 "GitGate").
//
// Phase 0.5 walking skeleton, stage 2 of 5 (bf-3a7): builds on stage 1 (bf-6d4) shim
// dispatch + Config::hardcoded() and adds the eligibility gate that decides a run
// MAY go remote. This is the happy-path skeleton: the four checks run in order,
// each shelling out to system git (no libgit2 — matches predecessor behavior), and
// any failure returns a simple (eligible=false, reason). Full per-check reason
// strings, untracked-file nuance, and topology edge cases are Phase 1a (bf-4zh).
//
// No RunLog, no decision integration yet — just the gate function + tests.

use std::process::Command;

/// Eligibility result from GitGate.
///
/// The happy path is `eligible=true`; any failed check yields `eligible=false`
/// with a simple reason string. Phase 1a will expand each reason to specific,
/// actionable diagnostics (bf-4zh).
#[derive(Debug, Clone, PartialEq)]
pub struct Eligibility {
    /// True iff all four GitGate checks passed.
    pub eligible: bool,
    /// Human-readable reason when ineligible; empty string when eligible.
    pub reason: String,
}

impl Eligibility {
    /// The happy-path result: all checks passed.
    fn eligible() -> Self {
        Eligibility {
            eligible: true,
            reason: String::new(),
        }
    }

    /// A failed check: ineligible with a simple reason.
    fn ineligible(reason: &str) -> Self {
        Eligibility {
            eligible: false,
            reason: reason.to_string(),
        }
    }
}

/// Check whether cwd is inside a git work tree.
///
/// Runs `git rev-parse --is-inside-work-tree` and returns true iff the output
/// is "true". This is the first GitGate check (plan Components §3 "GitGate").
/// Shells out to system git; never uses libgit2 (predecessor behavior contract).
pub fn is_inside_work_tree() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git rev-parse --is-inside-work-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "true")
}

/// Check whether HEAD resolves.
///
/// Runs `git rev-parse --verify HEAD` and returns true iff it succeeds (exit
/// code 0). This catches repos with no commits (HEAD unborn) and other malformed
/// states. Shells out to system git; never uses libgit2.
pub fn head_resolves() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    Ok(output.status.success())
}

/// Check whether a configured remote exists.
///
/// Runs `git remote get-url <ci_remote>` and returns true iff it succeeds.
/// The remote name comes from `config.ci_remote` (default: "origin"). Shells out
/// to system git; never uses libgit2.
pub fn remote_exists(ci_remote: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["remote", "get-url", ci_remote])
        .output()
        .map_err(|e| format!("git remote failed: {e}"))?;

    Ok(output.status.success())
}

/// Check whether the working tree is clean (no uncommitted changes).
///
/// Runs `git status --porcelain` and returns true iff the output is empty.
/// An empty output means no tracked files have changes and no untracked files
/// are present (vanilla porcelain mode; Phase 1a adds untracked-file nuance).
/// Shells out to system git; never uses libgit2.
pub fn is_tree_clean() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git status --porcelain failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().is_empty())
}

/// Run all four GitGate eligibility checks in order and return the result.
///
/// The checks run sequentially (plan Components §3 "GitGate"), stopping at the
/// first failure:
///
/// 1. Inside a work tree (`git rev-parse --is-inside-work-tree`)
/// 2. HEAD resolves (`git rev-parse --verify HEAD`)
/// 3. Configured remote exists (`git remote get-url <ci_remote>`)
/// 4. Clean tree (`git status --porcelain` empty)
///
/// The happy path (all four pass) yields `eligible=true`. Any failed check yields
/// `eligible=false` with a simple reason string. Phase 1a (bf-4zh) expands each
/// reason to specific, actionable diagnostics and adds untracked-file nuance.
///
/// This function only answers the eligibility question; it does not integrate
/// with the decision engine, RunLog, or any pipeline stage. Integration lands
/// in later beads.
pub fn check_git_gate(ci_remote: &str) -> Eligibility {
    // Check 1: inside a work tree. If we're not even in a git repo, nothing
    // else makes sense — fail fast with a clear reason.
    match is_inside_work_tree() {
        Ok(true) => {}
        Ok(false) => return Eligibility::ineligible("not inside a git work tree"),
        Err(e) => panic!("GitGate check 1 failed: {e}"),
    }

    // Check 2: HEAD resolves. Catches repos with no commits (HEAD unborn) and
    // other malformed states.
    match head_resolves() {
        Ok(true) => {}
        Ok(false) => return Eligibility::ineligible("HEAD does not resolve (no commits?)"),
        Err(e) => panic!("GitGate check 2 failed: {e}"),
    }

    // Check 3: configured remote exists. The ci_remote (default "origin") must
    // be configured or we have nowhere to push epoch refs.
    match remote_exists(ci_remote) {
        Ok(true) => {}
        Ok(false) => {
            return Eligibility::ineligible(&format!("remote '{ci_remote}' not configured"))
        }
        Err(e) => panic!("GitGate check 3 failed: {e}"),
    }

    // Check 4: clean tree. No tracked changes, no untracked files (vanilla
    // porcelain; Phase 1a adds nuance here).
    match is_tree_clean() {
        Ok(true) => {}
        Ok(false) => {
            return Eligibility::ineligible("working tree is not clean (uncommitted changes)")
        }
        Err(e) => panic!("GitGate check 4 failed: {e}"),
    }

    // All four checks passed: eligible for remote offload.
    Eligibility::eligible()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

    // Serialize tests that change the current directory to avoid interference
    static DIR_MUTEX: Mutex<()> = Mutex::new(());

    // --- filesystem fixtures ------------------------------------------------
    //
    // gate.rs stays std-only (no `tempfile` dev-dependency): TempDir is a minimal
    // stand-in, a uniquely-named subdir of std::env::temp_dir() removed wholesale
    // on drop. It is pure std, so it is cross-platform.

    /// A throwaway directory under the system temp, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(suffix: &str) -> Self {
            // Use a thread ID and a random component to avoid collisions in parallel tests
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
            // Ensure the directory doesn't already exist
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
            // Best-effort: a failed cleanup (the dir already gone) must not
            // fail the test or panic on drop.
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
    /// This is the happy-path fixture: a committed-clean repo with a configured
    /// remote that passes all four GitGate checks.
    fn setup_happy_repo() -> (TempDir, PathBuf) {
        let repo_dir = TempDir::new("happy-repo");
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

        (repo_dir, remote_dir.0.clone())
    }

    #[test]
    fn happy_repo_passes_all_checks() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory for the check
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            result.eligible,
            "happy repo should be eligible, got reason: {}",
            result.reason
        );
        assert!(
            result.reason.is_empty(),
            "happy repo should have empty reason, got: {}",
            result.reason
        );
    }

    #[test]
    fn repo_with_uncommitted_change_fails_clean_tree_check() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory and add an uncommitted change
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Modify a file without committing
        let test_file = repo_dir.path().join("test.txt");
        fs::write(&test_file, "modified content").expect("write modified file");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            !result.eligible,
            "repo with uncommitted change should not be eligible"
        );
        assert!(
            result.reason.contains("clean"),
            "reason should mention clean tree, got: {}",
            result.reason
        );
    }

    #[test]
    fn repo_without_remote_fails_remote_check() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let repo_dir = TempDir::new("no-remote");

        // Initialize a repo with no remote
        git_cmd(repo_dir.path(), &["init"]).expect("git init");
        git_cmd(repo_dir.path(), &["config", "user.name", "Test User"])
            .expect("git config user.name");
        git_cmd(
            repo_dir.path(),
            &["config", "user.email", "test@example.com"],
        )
        .expect("git config user.email");

        // Create a commit so HEAD resolves
        let test_file = repo_dir.path().join("test.txt");
        fs::write(&test_file, "test content").expect("write test file");
        git_cmd(repo_dir.path(), &["add", "."]).expect("git add");
        git_cmd(repo_dir.path(), &["commit", "-m", "Initial commit"]).expect("git commit");

        // Change into the repo directory for the check
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            !result.eligible,
            "repo without remote should not be eligible"
        );
        assert!(
            result.reason.contains("remote"),
            "reason should mention remote, got: {}",
            result.reason
        );
    }

    #[test]
    fn repo_with_untracked_file_fails_clean_tree_check() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory and add an untracked file
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Create an untracked file
        let untracked_file = repo_dir.path().join("untracked.txt");
        fs::write(&untracked_file, "untracked content").expect("write untracked file");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            !result.eligible,
            "repo with untracked file should not be eligible"
        );
        assert!(
            result.reason.contains("clean"),
            "reason should mention clean tree, got: {}",
            result.reason
        );
    }

    #[test]
    fn eligibility_eligible_returns_true_with_empty_reason() {
        let eligible = Eligibility::eligible();
        assert!(eligible.eligible);
        assert!(eligible.reason.is_empty());
    }

    #[test]
    fn eligibility_ineligible_returns_false_with_reason() {
        let reason = "test reason";
        let ineligible = Eligibility::ineligible(reason);
        assert!(!ineligible.eligible);
        assert_eq!(ineligible.reason, reason);
    }
}
