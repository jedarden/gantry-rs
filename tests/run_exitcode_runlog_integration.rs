// gantry — integration tests for `gantry run`'s exit-code and RunLog contracts.
//
// Scope: the outcome fidelity of the explicit-offload pipeline — what exit
// code the caller gets and what the ledger records — on all three verdict
// rungs of the remote path plus the local capped fallback:
//   1. Local capped fallback: the wrapped command's exit code passes through
//      byte-exact (INV-3), for 0 and nonzero, on the gate-ineligible rung of
//      the ladder (the Tier-0 direct-local pin lives in
//      `run_args_integration.rs`; the deadline-driven rungs live in
//      `deadline_expiry_integration.rs`).
//   2. Remote path: the verdict ladder holds for run exactly as for
//      intercepted cargo (plan §"CLI surface": "remote success lands on the
//      verdict ladder exactly as an intercepted run does") — wait exit 0 is
//      Pass → 0, wait exit 1 is TestFailure → 1, and a terminal
//      InfraFailure verdict (wait exit ≥ 2, the ladder's infra signal) is
//      never a fabricated outcome: it is flight-recorded at the
//      remote-verdict stage and the run degrades through the capped-local
//      ladder, where the wrapped command earns the caller's exit for real.
//   3. RunLog: a run invocation records an intent carrying the wrapped argv
//      (program and tail), and wrapping `cargo test` produces the identical
//      ledger intent and wire submission an intercepted `cargo test`
//      produces (decision.rs's faithful-argv contract: "a backend cannot
//      tell the two submissions apart").
//
// Hermeticity (no network anywhere): the remote pipeline is driven only
// through the command backend with the mock-executor technique of
// `run_args_integration.rs` — a `GANTRY_EXEC_PATH` script that records the
// submit argv and exits a per-world-baked wait code — against a local bare
// remote. The `cargo`-wrapped comparison drives the real binary through a
// symlink named `cargo` (the `integration.rs` technique) with the shim's
// PATH pin (`realbin` first) from `deadline_expiry_integration.rs`. Each
// test builds its own throwaway world: temp HOME, temp git repo, bare
// `origin`, mock executor outside the repo — nothing shared with other
// tests or the invoking user's environment (gantry-275ec80c).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// The decision line the remote arm prints before the git gate.
const REMOTE_DECISION: &str = "[gantry] decision: remote execution eligible";

/// A self-contained world for one `gantry run` outcome pin: an isolated HOME
/// carrying a user config that opts into the command backend (trust boundary
/// S-2 puts that choice in the user layer), a temp git repo with a bare
/// `origin` and one committed file, and a mock executor standing in for the
/// remote backend at the `GANTRY_EXEC_PATH` seam
/// (`CommandConfig::default()` reads it).
struct RunWorld {
    /// Isolated HOME: the user config layer, the state dir, the runlog.
    _home: TempDir,
    /// The fixture repo directory (a plain git repo — no cargo project
    /// needed; on every rung this file pins, the wrapped command either
    /// never runs locally or is `sh`).
    project: PathBuf,
    /// The bare remote registered as `origin`.
    bare_remote: PathBuf,
    /// The fixed handle the mock submit emits for this world.
    handle: String,
    /// The mock executor (records the submit argv; wait exits the baked code).
    mock: MockExecutor,
    /// A directory holding `cargo` → the gantry binary, for the one test
    /// that drives the intercepted shape for comparison. Outside the repo,
    /// so the fixture tree stays clean.
    shim_dir: TempDir,
    /// A directory holding `cargo` → the real cargo, pinned first on PATH so
    /// any local resolution resolves the real toolchain, never the shim.
    realbin_dir: TempDir,
    /// Dropped last so the temp dirs outlive every Command run against them.
    _project_dir: TempDir,
}

/// A mock command-template executor: one script serving submit/wait/logs,
/// recording the argv each step was handed (the
/// `command_backend_integration.rs` technique, pointed at the
/// `GANTRY_EXEC_PATH` seam). The wait step exits a code baked at world
/// construction — that exit code IS the remote verdict under test (the
/// backend's wait contract: 0 pass, 1 test-failure, ≥2 infra).
struct MockExecutor {
    dir: TempDir,
    /// Path to the executable script (what `GANTRY_EXEC_PATH` is set to).
    script: PathBuf,
}

impl MockExecutor {
    /// Write the mock executor script. Submit records {repo}, {rev},
    /// {args_json} one per line and emits a fixed handle; wait records the
    /// handle and exits `wait_exit`; logs echoes one line. Scripts use
    /// `#!/usr/bin/env bash` (NixOS has no /bin/bash).
    fn new(handle: &str, wait_exit: i32) -> Self {
        let dir = TempDir::new().expect("create mock executor dir");
        let script = dir.path().join("mock-exec.sh");
        fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\ndir=\"{}\"\ncase \"$1\" in\n\
                 submit)\n  shift\n  printf '%s\\n' \"$@\" > \"$dir/submit-argv.txt\"\n  \
                 echo {handle}\n  ;;\n\
                 wait)\n  printf '%s\\n' \"$@\" > \"$dir/wait-argv.txt\"\n  exit {wait_exit}\n  ;;\n\
                 logs)\n  echo \"log line for $2\"\n  ;;\nesac\n",
                dir.path().display()
            ),
        )
        .expect("write mock executor script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("chmod mock executor");
        MockExecutor { dir, script }
    }

    /// The submit argv the pipeline handed the backend (one placeholder
    /// value per line). Panics when absent: the pipeline never reached
    /// submit, and every caller needs it to have.
    fn submit_argv(&self) -> Vec<String> {
        let path = self.dir.path().join("submit-argv.txt");
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read submit record at {}: {e}", path.display()))
            .lines()
            .map(String::from)
            .collect()
    }
}

impl RunWorld {
    /// Create the world. `name` namespaces the handle and fixture identities;
    /// `wait_exit` is the mock wait's exit code — the remote verdict under
    /// test (0 pass, 1 failure, ≥2 infra).
    fn new(name: &str, wait_exit: i32) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // User config layer: the command backend with no template table, so
        // build_backend falls through to the default templates — which read
        // the GANTRY_EXEC_PATH seam this world points at the mock.
        let config_dir = home.path().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("create user config dir");
        fs::write(
            config_dir.join("config.toml"),
            "[remote]\nbackend = \"command\"\n",
        )
        .expect("write user config opting into the command backend");

        fs::write(
            project.join("README.md"),
            format!("gantry run exit-code/runlog fixture ({name})\n"),
        )
        .expect("write fixture file");

        // Bare remote that accepts every push (no hook), INSIDE the fixture
        // work tree but gitignored rather than committed: an empty bare repo
        // is committed as plain files, so the first pipeline's push would
        // write objects/refs into it and the second run of the two-shape
        // world would refuse at the git gate on its own origin's untracked
        // mutation. Ignored files are invisible to the vanilla-porcelain
        // gate, so the world survives consecutive runs (the
        // run_args/deadline worlds gate before their single push and never
        // needed this).
        let bare_remote = project.join(".git-state/bare-remote.git");
        fs::write(project.join(".gitignore"), ".git-state/\n").expect("write fixture .gitignore");
        fs::create_dir_all(bare_remote.parent().unwrap()).expect("create .git-state/");
        git(&project, &["init", "--bare", bare_remote.to_str().unwrap()]);
        git(&project, &["init"]);
        git(&project, &["config", "user.name", "Run Exit Test"]);
        git(&project, &["config", "user.email", "run-exit@test.invalid"]);
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // The shim link (cargo → gantry) lives outside the repo: invoking it
        // gives argv[0]="cargo" without dirtying the fixture tree.
        let shim_dir = TempDir::new().expect("create shim dir");
        std::os::unix::fs::symlink(gantry_binary(), shim_dir.path().join("cargo"))
            .expect("create cargo→gantry symlink");

        // Pin the "real cargo" any local resolution finds (the deadline
        // drill's guard: the host PATH may carry a wrapper that offloads
        // cargo in a git repo with an origin — and this fixture is exactly
        // such a repo).
        let realbin_dir = TempDir::new().expect("create realbin dir");
        std::os::unix::fs::symlink(env!("CARGO"), realbin_dir.path().join("cargo"))
            .expect("symlink the real cargo into realbin");

        RunWorld {
            _home: home,
            project,
            bare_remote,
            handle: format!("handle-{name}"),
            mock: MockExecutor::new(&format!("handle-{name}"), wait_exit),
            shim_dir,
            realbin_dir,
            _project_dir: project_dir,
        }
    }

    fn home(&self) -> PathBuf {
        self._home.path().to_path_buf()
    }

    /// The fixed handle the mock submit emits for this world.
    fn handle(&self) -> String {
        self.handle.clone()
    }

    /// The gantry state dir under the isolated HOME (the runlog's home).
    fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/gantry")
    }

    /// The isolated runlog parsed leniently into JSON values, one per line.
    fn runlog_records(&self) -> Vec<serde_json::Value> {
        let path = self.state_dir().join("runs.jsonl");
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read runlog at {}: {e}", path.display()));
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("runlog line is not JSON ({e}): {line}"))
            })
            .collect()
    }

    /// The `rec: "intent"` records of the runlog.
    fn intent_records(&self) -> Vec<serde_json::Value> {
        self.runlog_records()
            .into_iter()
            .filter(|r| r["rec"] == "intent")
            .collect()
    }

    /// The `rec: "verdict"` (terminal) records of the runlog.
    fn verdict_records(&self) -> Vec<serde_json::Value> {
        self.runlog_records()
            .into_iter()
            .filter(|r| r["rec"] == "verdict")
            .collect()
    }

    /// The flight-recorder bundles under the state dir, sorted, one directory
    /// per recorded InfraFailure.
    fn crash_dirs(&self) -> Vec<PathBuf> {
        let crash_root = self.state_dir().join("crash");
        let mut dirs: Vec<PathBuf> = fs::read_dir(&crash_root)
            .unwrap_or_else(|e| panic!("read crash dir at {}: {e}", crash_root.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        dirs.sort();
        dirs
    }

    /// Isolation env shared by every gantry child this world spawns:
    /// throwaway HOME, no XDG redirects, no inherited GANTRY_* knob except
    /// the mock-executor seam, no shared target dir, realbin first on PATH.
    fn isolate(&self, cmd: &mut Command) {
        cmd.env("HOME", self.home());
        cmd.env_remove("XDG_CONFIG_HOME");
        cmd.env_remove("XDG_STATE_HOME");
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().to_string();
            if key.starts_with("GANTRY_") {
                cmd.env_remove(&key);
            }
        }
        cmd.env("GANTRY_EXEC_PATH", &self.mock.script);
        cmd.env_remove("CARGO_TARGET_DIR");
        cmd.env_remove("CARGO_BUILD_TARGET_DIR");
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                self.realbin_dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }

    /// Run the binary as an explicit offload in the fixture: `gantry run --
    /// <wrapped…>`, backend taken from the user config (the override path is
    /// `run_args_integration.rs`'s scope).
    fn run_offload(&self, wrapped: &[&str]) -> (i32, String, String) {
        let mut argv = vec!["run", "--"];
        argv.extend_from_slice(wrapped);
        let mut cmd = Command::new(gantry_binary());
        cmd.current_dir(&self.project).args(&argv);
        self.isolate(&mut cmd);
        let output = cmd.output().expect("run gantry run offload");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// Run the binary AS the cargo shim in the fixture (`cargo <args…>` via
    /// the symlink), for the intercepted-shape comparison.
    fn run_shim(&self, args: &[&str]) -> (i32, String, String) {
        let mut cmd = Command::new(self.shim_dir.path().join("cargo"));
        cmd.current_dir(&self.project).args(args);
        self.isolate(&mut cmd);
        let output = cmd.output().expect("run cargo shim");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }
}

/// Run a git subcommand in `dir`, panicking with its stderr on failure.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ============================================================================
// Remote path, pass rung: wait 0 → Pass → exit 0, ledger says ran=remote.
// ============================================================================

#[test]
fn remote_pass_rung_returns_exit_0_and_records_the_remote_verdict() {
    let world = RunWorld::new("rung-pass", 0);
    let (code, _stdout, stderr) = world.run_offload(&["echo", "remote-pass-rung"]);

    assert_eq!(code, 0, "the pass rung must exit 0, stderr:\n{stderr}");
    assert!(
        stderr.contains(REMOTE_DECISION),
        "the run must enter the remote pipeline, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the pass rung must land the Pass trailer, got:\n{stderr}"
    );

    // The ledger's one terminal record is the remote verdict itself: pass,
    // ran=remote, the faithful exit, under the backend handle the mock
    // emitted — and it closes the intent's run id.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record: {verdicts:?}"
    );
    assert_eq!(verdicts[0]["verdict"], "pass");
    assert_eq!(verdicts[0]["ran"], "remote");
    assert_eq!(verdicts[0]["exit_code"], 0);
    assert_eq!(verdicts[0]["handle"], world.handle());
    let intents = world.intent_records();
    assert_eq!(intents.len(), 1, "exactly one intent record: {intents:?}");
    assert_eq!(
        verdicts[0]["run_id"], intents[0]["run_id"],
        "the verdict must close the intent's run id"
    );
}

// ============================================================================
// Remote path, failure rung: wait 1 → TestFailure → exit 1.
// ============================================================================

#[test]
fn remote_failure_rung_returns_exit_1_and_records_the_test_failure() {
    let world = RunWorld::new("rung-fail", 1);
    let (code, _stdout, stderr) = world.run_offload(&["sh", "-c", "exit 0"]);

    assert_eq!(code, 1, "the failure rung must exit 1, stderr:\n{stderr}");
    assert!(
        stderr.contains("[gantry] verdict: TestFailure"),
        "the failure rung must land the TestFailure trailer, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] falling back to capped local run"),
        "a terminal TestFailure is the answer — no local rerun may follow, \
         got:\n{stderr}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record: {verdicts:?}"
    );
    assert_eq!(verdicts[0]["verdict"], "test_failure");
    assert_eq!(verdicts[0]["ran"], "remote");
    assert_eq!(verdicts[0]["exit_code"], 1);
}

// ============================================================================
// Remote path, infra rung: wait ≥2 is the InfraFailure signal — flight-
// recorded, never fabricated, and the capped-local ladder earns the exit.
// ============================================================================

/// The infra rung of the ladder, with a wrapped command that fails locally:
/// the remote attempt is flight-recorded at the remote-verdict stage (the
/// same classification home the intercepted tail uses for a terminal
/// InfraFailure verdict), the run degrades through the capped-local ladder
/// instead of ending the caller's request, and the locally earned exit — 5,
/// byte-exact — is what the caller gets (INV-3). Nothing fabricated: the
/// ledger's one terminal record is the ladder's own local outcome.
#[test]
fn remote_infra_rung_degrades_to_the_ladder_with_the_wrapped_failing_exit() {
    let world = RunWorld::new("rung-infra-fail", 2);
    let (code, _stdout, stderr) = world.run_offload(&["sh", "-c", "exit 5"]);

    assert_eq!(
        code, 5,
        "the caller's exit is the wrapped command's own, byte-exact, earned \
         by the capped-local rerun, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] infra: remote run ended in InfraFailure"),
        "the degrade must name the infra rung as its reason, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the infra rung must engage the capped-local ladder, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: TestFailure"),
        "the trailer must report the local rerun's earned verdict, got:\n{stderr}"
    );

    // The remote attempt's classification home: the remote-verdict bundle,
    // written even though wait() itself returned cleanly.
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the infra rung is flight-recorded exactly once, got: {crash_dirs:?}"
    );
    let events = fs::read_to_string(crash_dirs[0].join("events.jsonl"))
        .expect("read the bundle's events.jsonl");
    assert!(
        events.contains("\"stage\":\"remote-verdict\""),
        "the bundle names the remote-verdict stage, got:\n{events}"
    );

    // The ledger's one terminal record is the ladder's, not a remote verdict
    // for the abandoned attempt and never a fabricated pass.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "test_failure",
        "the ledger records the local rerun's own failing verdict"
    );
    assert_eq!(verdicts[0]["ran"], "local_after_infra");
    assert_eq!(verdicts[0]["exit_code"], 5);
}

/// The same rung with a wrapped command that succeeds locally: the ladder's
/// locally earned pass IS a legitimate answer (a real result, not a
/// fabricated one — the deadline drill's no-fabrication pin, held from the
/// passing side at the binary level).
#[test]
fn remote_infra_rung_with_a_passing_wrapped_command_earns_its_local_pass() {
    let world = RunWorld::new("rung-infra-pass", 2);
    let (code, stdout, stderr) = world.run_offload(&["echo", "infra-then-local-pass"]);

    assert_eq!(
        code, 0,
        "the locally earned pass is the answer, stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("infra-then-local-pass"),
        "the capped-local rerun must run the wrapped command for real, got:\n{stdout}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the trailer must report the locally earned verdict, got:\n{stderr}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record: {verdicts:?}"
    );
    assert_eq!(verdicts[0]["verdict"], "pass");
    assert_eq!(verdicts[0]["ran"], "local_after_infra");
    assert_eq!(verdicts[0]["exit_code"], 0);
}

// ============================================================================
// Local capped fallback: the wrapped exit passes through byte-exact on the
// gate-ineligible rung, for nonzero and 0.
// ============================================================================

/// A gate-ineligible run never fails the caller: the fallback ladder takes
/// over and the wrapped command's exit code — 9 here — passes through
/// byte-exact (INV-3). The mock backend is never reached; the cap and the
/// ledger treatment are the same ladder every other rung uses.
#[test]
fn gate_ineligible_fallback_passes_a_nonzero_wrapped_exit_through_byte_exact() {
    let world = RunWorld::new("gate-fallback-nonzero", 0);
    fs::write(world.project.join("dirty.txt"), "uncommitted\n").expect("dirty the tree");

    let (code, _stdout, stderr) = world.run_offload(&["sh", "-c", "exit 9"]);

    assert!(
        stderr.contains("[gantry] ineligible:"),
        "the dirty tree must refuse the run at the git gate, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the documented fallback ladder must take over, got:\n{stderr}"
    );
    assert_eq!(
        code, 9,
        "the wrapped exit must pass through byte-exact, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: TestFailure"),
        "a nonzero wrapped exit is the command's failure, not infra, got:\n{stderr}"
    );

    // The ledger's one terminal record is the ladder's local outcome.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record: {verdicts:?}"
    );
    assert_eq!(verdicts[0]["ran"], "local_after_infra");
    assert_eq!(verdicts[0]["exit_code"], 9);
    assert!(
        !world.mock.dir.path().join("submit-argv.txt").exists(),
        "the gate refusal must happen before any backend contact"
    );
}

/// The zero half of the same contract: a gate-ineligible run whose wrapped
/// command succeeds passes 0 through byte-exact — the ladder's real local
/// outcome, not a defaulted success.
#[test]
fn gate_ineligible_fallback_passes_a_zero_wrapped_exit_through_byte_exact() {
    let world = RunWorld::new("gate-fallback-zero", 0);
    fs::write(world.project.join("dirty.txt"), "uncommitted\n").expect("dirty the tree");

    let (code, stdout, stderr) = world.run_offload(&["echo", "gate-then-local-pass"]);

    assert!(
        stderr.contains("[gantry] ineligible:"),
        "the dirty tree must refuse the run at the git gate, got:\n{stderr}"
    );
    assert_eq!(
        code, 0,
        "the wrapped command's success must pass through byte-exact, stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("gate-then-local-pass"),
        "the fallback must run the wrapped command for real, got:\n{stdout}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the trailer must report the locally earned verdict, got:\n{stderr}"
    );
}

// ============================================================================
// RunLog: the wrapped argv rides the intent, and a cargo-wrapped run is
// indistinguishable from the intercepted submission.
// ============================================================================

/// A run invocation records a ledger intent carrying the wrapped argv —
/// program as `tool`, tail as `args` — addressed to the same repo identity
/// the intercepted pipeline records, with the remote decision and the
/// configured backend named.
#[test]
fn runlog_intent_carries_the_wrapped_argv() {
    let world = RunWorld::new("runlog-argv", 0);
    let (code, _stdout, stderr) = world.run_offload(&["echo", "run-runlog-pin"]);

    assert_eq!(code, 0, "the run must pass, stderr:\n{stderr}");
    assert!(
        stderr.contains(REMOTE_DECISION),
        "the run must enter the remote pipeline, got:\n{stderr}"
    );

    let intents = world.intent_records();
    assert_eq!(intents.len(), 1, "exactly one intent record: {intents:?}");
    assert_eq!(
        intents[0]["tool"], "echo",
        "the intent's tool is the wrapped program"
    );
    assert_eq!(
        intents[0]["args"],
        serde_json::json!(["run-runlog-pin"]),
        "the intent's args are the wrapped tail, verbatim"
    );
    assert_eq!(intents[0]["decision"], "remote");
    assert_eq!(intents[0]["backend"], "command");
    assert_eq!(
        intents[0]["repo"].as_str().unwrap_or(""),
        format!("file://{}", world.bare_remote.display()),
        "the intent must address the same repo identity the intercepted \
         pipeline records"
    );
    assert!(
        intents[0]["sha"].as_str().is_some_and(|s| s.len() == 40),
        "the intent must carry the HEAD sha, got: {}",
        intents[0]["sha"]
    );
}

/// The faithful-argv contract, end to end: `gantry run -- cargo test
/// --nocapture` produces the identical wire submission and the identical
/// ledger intent an intercepted `cargo test --nocapture` produces — a
/// backend cannot tell the two submissions apart, and `gantry why` replays
/// them identically. Both shapes are driven for real through the same world
/// (the shim link for the intercepted half, the management CLI for the run
/// half) and compared at both boundaries.
#[test]
fn cargo_wrapped_run_matches_the_intercepted_submission() {
    let world = RunWorld::new("runlog-cargo", 0);

    // Half one: the intercepted shape. `cargo test --nocapture` through the
    // shim — the mock wait passes, so the suite never runs anywhere and the
    // round trip is fast.
    let (code, _stdout, stderr) = world.run_shim(&["test", "--nocapture"]);
    assert_eq!(code, 0, "the intercepted run must pass, stderr:\n{stderr}");
    let intercepted_argv = world.mock.submit_argv();

    // Half two: the explicit shape — the same wrapped command through
    // `gantry run`.
    let (code, _stdout, stderr) = world.run_offload(&["cargo", "test", "--nocapture"]);
    assert_eq!(code, 0, "the wrapped run must pass, stderr:\n{stderr}");
    let wrapped_argv = world.mock.submit_argv();

    // The wire boundary: byte-identical submissions — same repo, same rev,
    // and the cargo subcommand split off exactly as the shim splits it (the
    // faithful-argv contract's cargo carve-out), leaving args_json
    // `["--nocapture"]`.
    assert_eq!(
        wrapped_argv, intercepted_argv,
        "the two submissions must be indistinguishable at the wire, got \
         wrapped {wrapped_argv:?} vs intercepted {intercepted_argv:?}"
    );
    assert_eq!(
        wrapped_argv.len(),
        3,
        "repo, rev, args_json: {wrapped_argv:?}"
    );
    assert_eq!(
        wrapped_argv[2], "[\"--nocapture\"]",
        "the cargo tail must ride args_json exactly as the intercepted \
         submission carries it, got: {}",
        wrapped_argv[2]
    );

    // The ledger boundary: two intents (one per shape) with the identical
    // treatment — tool "cargo", the wrapped argv whole as args, same repo,
    // same sha, same decision and backend. If either path recorded a
    // different shape, fewer than two intents would match.
    let intents = world.intent_records();
    assert_eq!(intents.len(), 2, "one intent per shape: {intents:?}");
    let expected_args = serde_json::json!(["test", "--nocapture"]);
    for intent in &intents {
        assert_eq!(intent["tool"], "cargo", "intent: {intent}");
        assert_eq!(intent["args"], expected_args, "intent: {intent}");
        assert_eq!(intent["decision"], "remote", "intent: {intent}");
        assert_eq!(intent["backend"], "command", "intent: {intent}");
    }
    assert_eq!(
        intents[0]["repo"], intents[1]["repo"],
        "both shapes must address the same repo identity"
    );
    assert_eq!(
        intents[0]["sha"], intents[1]["sha"],
        "both shapes must record the same commit"
    );

    // And both terminal records are remote passes under the same handle.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        2,
        "one terminal record per shape: {verdicts:?}"
    );
    for verdict in &verdicts {
        assert_eq!(verdict["verdict"], "pass", "verdict: {verdict}");
        assert_eq!(verdict["ran"], "remote", "verdict: {verdict}");
        assert_eq!(verdict["exit_code"], 0, "verdict: {verdict}");
        assert_eq!(verdict["handle"], world.handle(), "verdict: {verdict}");
    }
}
