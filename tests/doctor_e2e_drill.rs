// gantry — integration tests for `gantry doctor --e2e` and `--drill`
// (plan Component 8, bf-2qq).
//
// The canary round-trips a real backend (push → clone → contract → verdict)
// against a fixture repo with the reference executor, and — the point of the
// negative test — FAILS when the backend is broken: unlike the decision
// pipeline, the canary has no fallback ladder to mask a dead backend with.
//
// The drill drives the real degrade chain through the drill-scoped injection
// hook (armed only inside the spawned `__drill-run` child) and asserts the
// parent reports every link: banner, drill-named reason, intent/verdict
// records, capped local run, faithful exit code — and refuses to pass when
// the chain never fired.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

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

/// Run a git command, failing the test on a non-zero exit.
fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a tiny committable cargo project with a bare `origin` remote,
/// mirroring the walking-skeleton fixtures: a clean work tree whose HEAD the
/// pipeline can push an epoch ref from.
fn make_project() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create project tempdir");
    let repo = dir.path().join("proj");
    fs::create_dir_all(repo.join("src")).expect("create src dir");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"canary-proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(repo.join("src/lib.rs"), "pub fn one() -> u32 {\n    1\n}\n").expect("write lib.rs");
    // Keep the work tree clean for GitGate once the runtime state dirs land.
    fs::write(repo.join(".gitignore"), "/target\n.git-state/\n").expect("write .gitignore");

    let state_dir = repo.join(".git-state");
    fs::create_dir_all(&state_dir).expect("create git state dir");
    let remote_dir = state_dir.join("bare-remote.git");
    git(
        dir.path(),
        &["init", "--bare", remote_dir.to_str().unwrap()],
    );

    git(&repo, &["init"]);
    git(&repo, &["config", "user.name", "gantry-test"]);
    git(
        &repo,
        &["config", "user.email", "gantry-test@example.invalid"],
    );
    git(
        &repo,
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial commit"]);
    (dir, repo)
}

/// An isolated HOME preconfigured to opt into the command backend (the user
/// config layer — trust boundary S-2), giving the run its own state dir so
/// the ledger never touches the invoking user's.
fn isolated_home() -> TempDir {
    let home = TempDir::new().expect("create isolated HOME");
    let config_dir = home.path().join(".config/gantry");
    fs::create_dir_all(&config_dir).expect("create user config dir");
    fs::write(
        config_dir.join("config.toml"),
        "[remote]\nbackend = \"command\"\n",
    )
    .expect("write user config opting into the command backend");
    home
}

/// Run the real gantry binary with the isolated-HOME environment the rest of
/// the integration suite uses: config and state under `home`, toolchain
/// paths pinned (the executor runs the real toolchain inside its fetched
/// worktree, and a rustup proxy needs CARGO_HOME/RUSTUP_HOME to resolve under
/// a redirected HOME), XDG overrides stripped, and kill-switch variables
/// cleared — the drill must reach the remote arm even where the invoking
/// environment forced gantry local.
fn run_gantry(
    home: &TempDir,
    cwd: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let orig_home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| orig_home.join(".cargo"));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| orig_home.join(".rustup"));
    let home_cargo = cargo_home.join("bin").join("cargo");

    let mut cmd = Command::new(gantry_binary());
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home.path())
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .envs(home_cargo.is_file().then(|| {
            (
                "GANTRY_EXEC_CARGO",
                home_cargo.to_string_lossy().into_owned(),
            )
        }))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("GANTRY_LOCAL");

    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let output = cmd.output().expect("spawn gantry");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn e2e_canary_round_trips_a_pass_verdict() {
    let (_dir, repo) = make_project();
    let home = isolated_home();
    let exec_state = TempDir::new().expect("create exec state dir");

    let (exit, stdout, stderr) = run_gantry(
        &home,
        &repo,
        &["doctor", "--e2e"],
        &[
            ("GANTRY_EXEC_PATH", executor_path().as_str()),
            ("GANTRY_EXEC_STATE_DIR", exec_state.path().to_str().unwrap()),
        ],
    );

    assert_eq!(
        exit, 0,
        "canary must pass against a working backend; stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("E2E test:"),
        "canary report missing; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("verdict Pass"),
        "the round-tripped verdict must be Pass; stdout:\n{stdout}"
    );
    assert!(
        stderr.contains("[gantry] e2e: submitted"),
        "canary must show its submission; stderr:\n{stderr}"
    );
}

#[test]
fn e2e_canary_fails_loudly_when_the_backend_is_broken() {
    // The anti-mask test: the canary has no fallback ladder, so a backend
    // that cannot even submit must fail the canary — never degrade into a
    // capped local pass (plan Component 8: "is the pipeline actually
    // working" must have a false answer for `false`).
    let (_dir, repo) = make_project();
    let home = isolated_home();
    let exec_state = TempDir::new().expect("create exec state dir");
    let broken_executor = repo.join("no-such-executor.sh");
    let broken = broken_executor.to_str().unwrap().to_string();

    let (exit, stdout, stderr) = run_gantry(
        &home,
        &repo,
        &["doctor", "--e2e"],
        &[
            ("GANTRY_EXEC_PATH", broken.as_str()),
            ("GANTRY_EXEC_STATE_DIR", exec_state.path().to_str().unwrap()),
        ],
    );

    assert_ne!(
        exit, 0,
        "a broken backend must fail the canary; stdout:\n{stdout}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("submit failed"),
        "the failure must name the broken stage; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("verdict Pass"),
        "a broken backend must never produce a Pass report; stdout:\n{stdout}"
    );
}

#[test]
fn drill_fires_the_full_degrade_chain() {
    let (_dir, repo) = make_project();
    let home = isolated_home();

    let (exit, stdout, stderr) = run_gantry(&home, &repo, &["doctor", "--drill"], &[]);

    assert_eq!(
        exit, 0,
        "the drill must pass on a full chain; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Fire drill:"),
        "drill report missing; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("the drill backend took the submission"),
        "link report missing; stdout:\n{stdout}"
    );

    // The child's chain is echoed through — assert the load-bearing lines
    // the parent claims it saw.
    assert!(
        stderr.contains("[gantry] submitted: gantry-drill-synthetic"),
        "the injection must take the submission; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] timeout: run gantry-drill-synthetic exceeded its deadline"),
        "the synthetic failure must raise the timeout banner; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the ladder must take over; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the capped local run must land a Pass; stderr:\n{stderr}"
    );
}

#[test]
fn drill_reports_a_chain_that_did_not_fire() {
    // Outside any git repo the run never goes remote, so the synthetic
    // failure never reaches a wait boundary: the drill must FAIL, naming
    // what did not fire — never rubber-stamp a chain that stayed cold.
    let dir = TempDir::new().expect("create bare cwd");
    let home = isolated_home();

    let (exit, stdout, stderr) = run_gantry(&home, dir.path(), &["doctor", "--drill"], &[]);

    assert_ne!(
        exit, 0,
        "a chain that never fired must fail the drill; stdout:\n{stdout}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("did not fully fire"),
        "the report must name the missing links; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] timeout: run gantry-drill-synthetic"),
        "no banner may be claimed where no injection fired; stderr:\n{stderr}"
    );
}
