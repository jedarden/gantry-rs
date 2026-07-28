// gantry — GitGate eligibility checks (plan Components §3 "GitGate").
//
// Phase 1a implementation (bf-4zh): detailed reasons + exact next actions (AS-4),
// edge cases EC-01..EC-05 (worktrees, detached HEAD, submodules, shallow clones,
// slow gate hint), linked-worktree correctness via git-common-dir.
//
// All git interaction shells out to system git (no libgit2 — matches predecessor
// behavior, honors user's git config/credentials/hooks).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Eligibility result from GitGate.
///
/// The happy path is `eligible=true`; any failed check yields `eligible=false`
/// with a detailed reason string and exact next action (AS-4). Phase 1a adds
/// actionable diagnostics and topology edge case handling (bf-4zh).
#[derive(Debug, Clone, PartialEq)]
pub struct Eligibility {
    /// True iff all four GitGate checks passed.
    pub eligible: bool,
    /// Human-readable reason when ineligible; empty string when eligible.
    pub reason: String,
    /// Exact next action for the user to take when ineligible.
    pub next_action: String,
    /// Whether HEAD is detached (EC-02). Eligible, but noted in decision output.
    pub is_detached: bool,
}

impl Eligibility {
    /// The happy-path result: all checks passed.
    fn eligible() -> Self {
        Eligibility {
            eligible: true,
            reason: String::new(),
            next_action: String::new(),
            is_detached: false,
        }
    }

    /// A failed check: ineligible with a detailed reason and next action.
    fn ineligible(reason: &str, next_action: &str) -> Self {
        Eligibility {
            eligible: false,
            reason: reason.to_string(),
            next_action: next_action.to_string(),
            is_detached: false,
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

/// Get the git common dir for worktree detection (EC-01).
///
/// Runs `git rev-parse --git-common-dir` to get the shared .git directory
/// for linked worktrees. This is used for JoinTable and memoization keys
/// so two worktrees of one repo dedup correctly.
pub fn git_common_dir() -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git rev-parse --git-common-dir failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check whether HEAD is detached (EC-02).
///
/// Runs `git rev-parse --abbrev-ref HEAD` and returns true iff the output
/// is "HEAD", indicating detached HEAD state. Detached HEAD is eligible for
/// remote offload (a sha is a sha), but noted in decision output.
pub fn is_head_detached() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git rev-parse --abbrev-ref HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "HEAD")
}

/// Check whether the repository has submodules (EC-03).
///
/// Returns true if `.gitmodules` exists in the repository root.
/// Submodules are not yet supported in the remote contract, so this causes
/// a local fallback with a clear reason.
pub fn has_submodules() -> Result<bool, String> {
    // First, get the repository root
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git rev-parse --show-toplevel failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let repo_root = String::from_utf8_lossy(&output.stdout);
    let gitmodules_path = Path::new(repo_root.trim()).join(".gitmodules");

    Ok(gitmodules_path.exists())
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

/// Check if there are any tracked or untracked changes (detailed version for AS-4).
///
/// Returns a tuple of (has_tracked_changes, has_untracked_files) so we can give
/// the user precise guidance on what to do next.
fn tree_clean_details() -> Result<(bool, bool), String> {
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
    let mut has_tracked_changes = false;
    let mut has_untracked_files = false;

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let first_char = line.chars().next().unwrap_or(' ');
        let second_char = line.chars().nth(1).unwrap_or(' ');

        // porcelain format: XY filename where X = staged, Y = unstaged
        // ?? means untracked
        if first_char == '?' || second_char == '?' {
            has_untracked_files = true;
        } else {
            has_tracked_changes = true;
        }
    }

    Ok((has_tracked_changes, has_untracked_files))
}

/// Run all four GitGate eligibility checks in order and return the result.
///
/// The checks run sequentially (plan Components §3 "GitGate"), stopping at the
/// first failure:
///
/// 1. Inside a work tree (`git rev-parse --is-inside-work-tree`)
/// 2. HEAD resolves (`git rev-parse --verify HEAD`)
/// 3. Configured remote exists (`git remote get-url <ci_remote>`)
/// 4. Clean tree (`git status --porcelain` empty) with detailed diagnostics (AS-4)
///
/// Edge cases handled (EC-01..EC-05):
/// - EC-01: Linked worktrees supported via git-common-dir detection
/// - EC-02: Detached HEAD is eligible (a sha is a sha)
/// - EC-03: Submodules cause local fallback with clear reason
/// - EC-04: Shallow clones will fail at push time (InfraFailure)
/// - EC-05: Slow gate (>2s) emits a hint
///
/// Phase 1a (bf-4zh) expands each reason to specific, actionable diagnostics
/// with exact next actions (AS-4).
///
/// This function only answers the eligibility question; it does not integrate
/// with the decision engine, RunLog, or any pipeline stage. Integration lands
/// in later beads.
pub fn check_git_gate(ci_remote: &str) -> Eligibility {
    let gate_start = Instant::now();

    // Check 1: inside a work tree. If we're not even in a git repo, nothing
    // else makes sense — fail fast with a clear reason.
    match is_inside_work_tree() {
        Ok(true) => {}
        Ok(false) => {
            return Eligibility::ineligible(
                "not inside a git work tree",
                "run this command from within a git repository",
            )
        }
        Err(e) => panic!("GitGate check 1 failed: {e}"),
    }

    // EC-01: Linked worktree support - detect git-common-dir for JoinTable keys
    let _common_dir = match git_common_dir() {
        Ok(dir) => dir,
        Err(e) => panic!("GitGate failed to detect git-common-dir: {e}"),
    };

    // Check 2: HEAD resolves. Catches repos with no commits (HEAD unborn) and
    // other malformed states.
    match head_resolves() {
        Ok(true) => {}
        Ok(false) => {
            return Eligibility::ineligible(
                "HEAD does not resolve (no commits yet)",
                "create an initial commit with 'git commit' before offloading",
            )
        }
        Err(e) => panic!("GitGate check 2 failed: {e}"),
    }

    // EC-02: Detached HEAD is eligible (a sha is a sha)
    let is_detached = match is_head_detached() {
        Ok(detached) => detached,
        Err(e) => panic!("GitGate failed to detect detached HEAD: {e}"),
    };

    // EC-03: Submodules cause local fallback (remote contract doesn't recurse yet)
    match has_submodules() {
        Ok(true) => {
            return Eligibility::ineligible(
                "repository contains submodules (not yet supported in remote contract)",
                "submodule support is planned; for now, run locally with GANTRY_LOCAL=1",
            )
        }
        Ok(false) => {}
        Err(e) => panic!("GitGate failed to check for submodules: {e}"),
    }

    // Check 3: configured remote exists. The ci_remote (default "origin") must
    // be configured or we have nowhere to push epoch refs.
    match remote_exists(ci_remote) {
        Ok(true) => {}
        Ok(false) => {
            return Eligibility::ineligible(
                &format!("remote '{ci_remote}' is not configured"),
                &format!(
                "add a remote with 'git remote add {ci_remote} <url>' or set ci_remote in config"
            ),
            )
        }
        Err(e) => panic!("GitGate check 3 failed: {e}"),
    }

    // Check 4: clean tree with detailed diagnostics (AS-4). Distinguish between
    // tracked changes and untracked files so the user knows exactly what to do.
    match tree_clean_details() {
        Ok((has_tracked, has_untracked)) => {
            if has_tracked {
                return Eligibility::ineligible(
                    "working tree has uncommitted changes to tracked files",
                    "commit or stash changes with 'git commit' or 'git stash' before offloading",
                );
            }
            if has_untracked {
                return Eligibility::ineligible(
                    "working tree has untracked files",
                    "commit files with 'git add <files> && git commit' or gitignore them",
                );
            }
        }
        Err(e) => panic!("GitGate check 4 failed: {e}"),
    }

    // EC-05: Slow gate hint - if the gate took >2s, emit a hint about .gitignore hygiene
    let gate_duration_ms = gate_start.elapsed().as_millis();
    if gate_duration_ms > 2000 {
        eprintln!(
            "[gantry] slow gate ({}ms) — consider .gitignore hygiene to reduce status check time",
            gate_duration_ms
        );
    }

    // All four checks passed: eligible for remote offload.
    let mut eligibility = Eligibility::eligible();
    eligibility.is_detached = is_detached;
    eligibility
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
        assert!(
            result.next_action.is_empty(),
            "happy repo should have empty next_action, got: {}",
            result.next_action
        );
        assert!(
            !result.is_detached,
            "happy repo should not be detached HEAD"
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
            result.reason.contains("tracked") || result.reason.contains("uncommitted"),
            "reason should mention tracked changes or uncommitted, got: {}",
            result.reason
        );
        assert!(
            !result.next_action.is_empty(),
            "next_action should not be empty for ineligibility, got: {}",
            result.next_action
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
        assert!(
            !result.next_action.is_empty(),
            "next_action should not be empty for ineligibility, got: {}",
            result.next_action
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
            result.reason.contains("untracked"),
            "reason should mention untracked files, got: {}",
            result.reason
        );
        assert!(
            !result.next_action.is_empty(),
            "next_action should not be empty for ineligibility, got: {}",
            result.next_action
        );
    }

    #[test]
    fn eligibility_eligible_returns_true_with_empty_reason() {
        let eligible = Eligibility::eligible();
        assert!(eligible.eligible);
        assert!(eligible.reason.is_empty());
        assert!(eligible.next_action.is_empty());
        assert!(!eligible.is_detached);
    }

    #[test]
    fn eligibility_ineligible_returns_false_with_reason() {
        let reason = "test reason";
        let next_action = "test action";
        let ineligible = Eligibility::ineligible(reason, next_action);
        assert!(!ineligible.eligible);
        assert_eq!(ineligible.reason, reason);
        assert_eq!(ineligible.next_action, next_action);
        assert!(!ineligible.is_detached);
    }

    #[test]
    fn detached_head_is_eligible_ec02() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Detach HEAD: checkout the current commit
        let sha = git_cmd(repo_dir.path(), &["rev-parse", "HEAD"]).expect("get sha");
        git_cmd(repo_dir.path(), &["checkout", &sha]).expect("detach HEAD");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            result.eligible,
            "detached HEAD should be eligible, got reason: {}",
            result.reason
        );
        assert!(
            result.is_detached,
            "is_detached should be true when HEAD is detached"
        );
    }

    #[test]
    fn repo_with_submodules_fails_ec03() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Create a .gitmodules file
        let gitmodules_path = repo_dir.path().join(".gitmodules");
        fs::write(
            &gitmodules_path,
            "[submodule \"example\"]\n    path = lib/example\n    url = https://github.com/example/example.git",
        )
        .expect("write .gitmodules");

        let result = check_git_gate("origin");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            !result.eligible,
            "repo with submodules should not be eligible"
        );
        assert!(
            result.reason.contains("submodule"),
            "reason should mention submodules, got: {}",
            result.reason
        );
        assert!(
            !result.next_action.is_empty(),
            "next_action should not be empty for ineligibility, got: {}",
            result.next_action
        );
    }

    #[test]
    fn git_common_dir_returns_valid_path_ec01() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        let common_dir = git_common_dir();

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");

        assert!(
            common_dir.is_ok(),
            "git_common_dir should succeed, got error: {:?}",
            common_dir
        );
        let dir = common_dir.unwrap();
        assert!(
            Path::new(&dir).exists(),
            "git_common_dir should return an existing path: {}",
            dir
        );
    }

    #[test]
    fn tree_clean_details_distinguishes_tracked_vs_untracked() {
        let _lock = DIR_MUTEX.lock().unwrap();
        let (repo_dir, _remote_dir) = setup_happy_repo();

        // Change into the repo directory
        let current_dir = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(repo_dir.path()).expect("set_current_dir");

        // Test clean tree
        let (tracked, untracked) = tree_clean_details().expect("clean tree check");
        assert!(!tracked && !untracked, "clean tree should have no changes");

        // Test with untracked file
        let untracked_file = repo_dir.path().join("untracked.txt");
        fs::write(&untracked_file, "untracked").expect("write untracked");
        let (tracked, untracked) = tree_clean_details().expect("untracked check");
        assert!(!tracked && untracked, "should have only untracked files");

        // Test with tracked change - remove untracked file first
        fs::remove_file(&untracked_file).expect("remove untracked file");
        let test_file = repo_dir.path().join("test.txt");
        fs::write(&test_file, "modified").expect("modify tracked file");
        let (tracked, untracked) = tree_clean_details().expect("tracked check");
        assert!(tracked && !untracked, "should have only tracked changes");

        // Restore the original directory
        std::env::set_current_dir(current_dir).expect("restore current_dir");
    }
}
