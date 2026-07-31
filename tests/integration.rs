// gantry — integration test proving cargo test round-trip (bf-1f9).
//
// Phase 0.5 exit criterion: run the real gantry binary AS the cargo shim against
// two tiny cargo-project fixtures (passing and failing suites), assert the round-
// trip through the local "remote" returns the correct exit code for both, verify
// no branch refs move (only refs/gantry/*), and check that stderr includes the
// decision line + verdict trailer.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// Serializes the fixture round-trips (see [`run_cargo_test`]).
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// Path to the gantry executor script.
fn executor_path() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("contrib/gantry-exec.sh");
    path.to_str().unwrap().to_string()
}

/// Path to the fixtures directory.
fn fixtures_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/integration/fixtures");
    path
}

/// Run gantry as a cargo shim in the given directory and return the result.
///
/// This simulates running `cargo test` through the gantry shim by:
/// 1. Setting up a git repo with a remote (for the walking skeleton)
/// 2. Creating a symlink from "cargo" to the gantry binary
/// 3. Running the symlinked binary (so argv[0]="cargo")
/// 4. Capturing exit code, stdout, and stderr
fn run_cargo_test(fixture_name: &str) -> (i32, String, String) {
    // The fixtures are shared, mutable state: every run re-creates the `cargo`
    // symlink and re-initializes the git repo in the same directory, so two tests
    // over one fixture race (an AlreadyExists symlink, a half-built repo). One
    // lock around the whole round-trip keeps them honest; the runs are fast.
    let _lock = FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let fixture_path = fixtures_dir().join(fixture_name);
    let gantry = gantry_binary();

    // Create a symlink named "cargo" that points to the gantry binary
    // This ensures argv[0] is "cargo" when the shim runs
    let cargo_link = fixture_path.join("cargo");
    let _ = std::fs::remove_file(&cargo_link); // Clean up any existing symlink
    std::os::unix::fs::symlink(&gantry, &cargo_link).expect("failed to create cargo symlink");

    // Initialize a git repo in the fixture with a remote
    // (required for the walking skeleton pipeline)
    let state_dir = fixture_path.join(".git-state");
    fs::create_dir_all(&state_dir).unwrap();

    let remote_dir = state_dir.join("bare-remote.git");
    Command::new("git")
        .args(["init", "--bare", remote_dir.to_str().unwrap()])
        .output()
        .expect("git init bare remote failed");

    Command::new("git")
        .current_dir(&fixture_path)
        .args(["init"])
        .output()
        .expect("git init failed");

    Command::new("git")
        .current_dir(&fixture_path)
        .args(["config", "user.name", "Test User"])
        .output()
        .expect("git config user.name failed");

    Command::new("git")
        .current_dir(&fixture_path)
        .args(["config", "user.email", "test@example.com"])
        .output()
        .expect("git config user.email failed");

    Command::new("git")
        .current_dir(&fixture_path)
        .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .output()
        .expect("git remote add failed");

    // Create .gitignore to exclude the state directory (keeps working tree clean for GitGate)
    let gitignore_path = fixture_path.join(".gitignore");
    fs::write(&gitignore_path, ".git-state/\n").expect("failed to write .gitignore");

    // Create an initial commit
    Command::new("git")
        .current_dir(&fixture_path)
        .args(["add", "."])
        .output()
        .expect("git add failed");

    Command::new("git")
        .current_dir(&fixture_path)
        .args(["commit", "-m", "Initial commit"])
        .output()
        .expect("git commit failed");

    // Record git refs before the run
    let before_refs = Command::new("git")
        .args(["ls-remote", remote_dir.to_str().unwrap()])
        .output()
        .expect("git ls-remote before failed");

    // Run the cargo symlink (which invokes gantry with argv[0]="cargo")
    let output = Command::new(&cargo_link)
        .current_dir(&fixture_path)
        .env("GANTRY_EXEC_PATH", executor_path())
        .args(["test"])
        .output()
        .expect("cargo test failed");

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Record git refs after the run
    let after_refs = Command::new("git")
        .args(["ls-remote", remote_dir.to_str().unwrap()])
        .output()
        .expect("git ls-remote after failed");

    // Verify ref invariants:
    // 1. No branch refs (refs/heads/*) should exist before or after
    // 2. Only refs/gantry/* refs should be created/modified
    let _before_stdout = String::from_utf8_lossy(&before_refs.stdout);
    let after_stdout = String::from_utf8_lossy(&after_refs.stdout);

    // Parse refs after the run
    let after_lines: Vec<&str> = after_stdout.lines().collect();

    for line in after_lines {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            parts.len(),
            2,
            "Each ls-remote line should have 2 parts: commit and ref"
        );

        let ref_name = parts[1];

        // Verify only refs/gantry/* refs exist
        assert!(
            ref_name.starts_with("refs/gantry/"),
            "Only refs/gantry/* refs should be created, found: {} (invariant INV-2)",
            ref_name
        );

        // Verify no other ref types exist
        assert!(
            !ref_name.starts_with("refs/heads/"),
            "No branch refs should exist, found: {} (invariant INV-2)",
            ref_name
        );
        assert!(
            !ref_name.starts_with("refs/tags/"),
            "No tag refs should exist, found: {}",
            ref_name
        );
        assert!(
            !ref_name.starts_with("refs/remotes/"),
            "No remote-tracking refs should exist directly, found: {}",
            ref_name
        );
    }

    (exit_code, stdout, stderr)
}

#[test]
fn test_passing_suite_round_trip_returns_exit_0() {
    let (exit_code, _stdout, stderr) = run_cargo_test("passing-suite");

    assert_eq!(
        exit_code, 0,
        "passing suite should exit with 0, got {}",
        exit_code
    );

    // Verify stderr includes decision line
    assert!(
        stderr.contains("[gantry] decision:"),
        "stderr should include decision line, got: {}",
        stderr
    );

    // Verify stderr includes verdict trailer
    assert!(
        stderr.contains("[gantry] verdict:"),
        "stderr should include verdict trailer, got: {}",
        stderr
    );
}

#[test]
fn test_failing_suite_round_trip_returns_non_zero_exit_code() {
    let (exit_code, _stdout, stderr) = run_cargo_test("failing-suite");

    assert_ne!(
        exit_code, 0,
        "failing suite should exit with non-zero, got {}",
        exit_code
    );

    // Verify stderr includes decision line
    assert!(
        stderr.contains("[gantry] decision:"),
        "stderr should include decision line, got: {}",
        stderr
    );

    // Verify stderr includes verdict trailer
    assert!(
        stderr.contains("[gantry] verdict:"),
        "stderr should include verdict trailer, got: {}",
        stderr
    );
}

#[test]
fn test_no_branch_refs_move_during_round_trip() {
    // This is asserted within run_cargo_test itself for both fixtures
    // Just run both fixtures to ensure the invariant holds
    run_cargo_test("passing-suite");
    run_cargo_test("failing-suite");
}

#[test]
fn test_only_gantry_refs_are_created_during_round_trip() {
    // Explicit test for invariant INV-2: only refs/gantry/* refs should be created/modified
    // This is already verified within run_cargo_test, but we make it explicit here
    let (_exit_code, _stdout, _stderr) = run_cargo_test("passing-suite");
    // If we get here without panicking, the invariant holds
}
