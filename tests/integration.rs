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

    // Assert no branch refs moved (only refs/gantry/* should exist)
    let before_stdout = String::from_utf8_lossy(&before_refs.stdout);
    let after_stdout = String::from_utf8_lossy(&after_refs.stdout);

    assert!(
        !before_stdout.contains("refs/heads/"),
        "No branch refs should exist before run"
    );
    assert!(
        !after_stdout.contains("refs/heads/"),
        "No branch refs should exist after run (invariant INV-2)"
    );

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
