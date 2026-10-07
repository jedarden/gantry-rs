// gantry — integration tests for `gantry run`'s argument surface.
//
// Scope: the argv contract of `gantry run [--backend B] -- <cmd…>` —
//   1. the wrapped argv parses program-first: everything after `--` is the
//      wrapped command verbatim, executed with the exit-code fidelity the
//      rest of gantry promises (INV-3);
//   2. usage errors exit 2 and print the usage line: a missing `--`
//      separator, an empty argv after `--`, and an unknown `--backend`
//      value (both GNU spellings, plus the dangling flag);
//   3. `--backend none|argo|command` overrides the configured backend at
//      the pipeline decision — in both directions.
//
// Hermeticity (no network anywhere):
//   - The remote pipeline is driven only through the command-template
//     backend with mock executor scripts (the `command_backend_integration`
//     technique) against a local bare remote — the same shape
//     `tests/integration.rs` proves end to end with the real executor.
//   - The argo override is asserted at the decision boundary only, in a
//     gate-ineligible world that stops the pipeline before any backend
//     (kubectl) contact: driving argo further would exec the host kubectl.
//   - The tests deliberately do NOT share the `tests/integration/fixtures`
//     cargo projects: integration.rs re-initializes those fixture trees in
//     place on every run, and a second test binary mutating the same tree
//     under parallel `cargo test` is exactly the shared-state race the
//     suite's hermeticity notes warn about (gantry-275ec80c). Each test
//     builds its own throwaway world instead, on the Tier0World pattern
//     from `tier0_integration.rs`: temp HOME, temp git repo, bare `origin`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// The usage line `cli::run` prints alongside every usage error — kept in
/// lockstep with the `USAGE` constant in `src/cli/run.rs`.
const USAGE_LINE: &str = "usage: gantry run [--backend none|argo|command] -- <cmd> [args…]";

/// The decision line the Tier-0 arm prints for a local run.
const TIER0_DECISION: &str = "[gantry] decision: local execution (Tier-0: no backend configured)";

/// The decision line printed when the pipeline is entered with a remote
/// backend — printed before the git gate, so it is the decision boundary
/// the `--backend` override must reach to count as "reaches the pipeline".
const REMOTE_DECISION: &str = "[gantry] decision: remote execution eligible";

/// A self-contained world for one `gantry run` invocation: a temp HOME (no
/// user config unless a test writes one, and the sandbox for the runlog and
/// state files) and a temp git repo with a bare `origin`. Nothing is shared
/// with other tests or with the invoking user's real environment.
struct RunWorld {
    /// Isolated HOME: the user config layer, the runlog, the state file.
    _home: TempDir,
    /// The fixture repo directory (a plain git repo — no cargo project
    /// needed; `gantry run` wraps arbitrary commands).
    project: PathBuf,
    /// The bare remote registered as `origin`.
    bare_remote: PathBuf,
    /// Holds the mock executor's script path; `None` until a test asks for
    /// the command backend. Only the path is kept — the executor itself is
    /// returned owned, so a test can read its records without aliasing the
    /// world borrow while calling `run`.
    mock: Option<PathBuf>,
    /// Dropped last so the temp dirs outlive every Command run against them.
    _project_dir: TempDir,
}

/// A mock command-template executor: one script serving submit/wait/logs,
/// recording the argv each step was handed so tests can assert the wire
/// shape (the `command_backend_integration.rs` technique, pointed at the
/// `GANTRY_EXEC_PATH` seam `CommandConfig::default()` reads).
struct MockExecutor {
    dir: TempDir,
    /// Path to the executable script (what `GANTRY_EXEC_PATH` is set to).
    script: PathBuf,
}

impl MockExecutor {
    /// Write the mock executor script. `submit` records its three arguments
    /// ({repo}, {rev}, {args_json}) one per line and emits a fixed handle;
    /// `wait` records the handle and exits 0 (verdict Pass); `logs` echoes
    /// one line. Scripts use `#!/usr/bin/env bash` (NixOS has no /bin/bash).
    fn new() -> Self {
        let dir = TempDir::new().expect("create mock executor dir");
        let script = dir.path().join("mock-exec.sh");
        fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\ndir=\"{}\"\ncase \"$1\" in\n\
                 submit)\n  shift\n  printf '%s\\n' \"$@\" > \"$dir/submit-argv.txt\"\n  \
                 echo run-explicit-1\n  ;;\n\
                 wait)\n  printf '%s\\n' \"$@\" > \"$dir/wait-argv.txt\"\n  exit 0\n  ;;\n\
                 logs)\n  echo \"log line for $2\"\n  ;;\nesac\n",
                dir.path().display()
            ),
        )
        .expect("write mock executor script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod mock executor");
        }
        MockExecutor { dir, script }
    }

    fn submit_record(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("submit-argv.txt"))
            .expect("mock submit record should exist (the pipeline reached submit)")
            .lines()
            .map(String::from)
            .collect()
    }
}

impl RunWorld {
    /// Create the world: temp HOME, temp git repo with a bare `origin` and
    /// one committed file, identity pinned repo-locally (a global gitconfig
    /// is not guaranteed under the isolated HOME).
    fn new(name: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        fs::write(
            project.join("README.md"),
            format!("gantry run argument-surface fixture ({name})\n"),
        )
        .expect("write fixture file");

        let bare_remote = project.join(".git-state/bare-remote.git");
        fs::create_dir_all(bare_remote.parent().unwrap()).expect("create .git-state/");
        git(&project, &["init", "--bare", bare_remote.to_str().unwrap()]);
        git(&project, &["init"]);
        git(&project, &["config", "user.name", "Run Args Test"]);
        git(&project, &["config", "user.email", "run-args@test.invalid"]);
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        RunWorld {
            _home: home,
            project,
            bare_remote,
            mock: None,
            _project_dir: project_dir,
        }
    }

    fn home(&self) -> PathBuf {
        self._home.path().to_path_buf()
    }

    /// The gantry state dir under the isolated HOME (the runlog's home).
    fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/gantry")
    }

    /// Write a user config layer into the isolated HOME — `[remote]
    /// backend = "<value>"` (the system layer cannot exist in a temp HOME).
    fn set_config_backend(&self, value: &str) {
        let config_dir = self.home().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("create user config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!("[remote]\nbackend = \"{value}\"\n"),
        )
        .expect("write user config");
    }

    /// Install the mock executor (and remember its script path for `run`);
    /// the executor is returned owned so record assertions never alias the
    /// world.
    fn mock_executor(&mut self) -> MockExecutor {
        let mock = MockExecutor::new();
        self.mock = Some(mock.script.clone());
        mock
    }

    /// All refs visible on the bare remote (empty string when none).
    fn remote_refs(&self) -> String {
        let out = Command::new("git")
            .args(["ls-remote", self.bare_remote.to_str().unwrap()])
            .output()
            .expect("git ls-remote");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Parse the isolated runlog into JSON values, one per JSONL line.
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

    /// Run the management CLI (`gantry <args…>`, invoked by absolute path —
    /// not the shim) inside the fixture repo, with the isolated HOME and
    /// every GANTRY_* knob stripped. `exec_path` sets `GANTRY_EXEC_PATH`
    /// (the mock executor seam) when the command backend is under test;
    /// it is stripped otherwise, so an inherited value from the invoking
    /// harness can never redirect the default template.
    fn run(&self, exec_path: Option<&Path>, args: &[&str]) -> (i32, String, String) {
        let mut cmd = Command::new(gantry_binary());
        cmd.current_dir(&self.project).args(args);
        cmd.env("HOME", self.home());
        cmd.env_remove("XDG_CONFIG_HOME");
        cmd.env_remove("XDG_STATE_HOME");
        cmd.env_remove("GANTRY_LOCAL");
        cmd.env_remove("GANTRY_ON");
        if let Some(path) = exec_path {
            cmd.env("GANTRY_EXEC_PATH", path);
        } else {
            cmd.env_remove("GANTRY_EXEC_PATH");
        }
        let output = cmd.output().expect("run gantry");
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
// Usage errors: exit 2, usage line on stderr, nothing else touched.
// ============================================================================

#[test]
fn missing_separator_is_a_usage_error() {
    // `gantry run make -j4`: a bare word before `--` is a missing separator,
    // rejected rather than guessed at — the words after it belong to the
    // wrapped command.
    let world = RunWorld::new("usage-no-separator");
    let (code, _stdout, stderr) = world.run(None, &["run", "make", "-j4"]);

    assert_eq!(code, 2, "missing separator must exit 2, stderr:\n{stderr}");
    assert!(
        stderr.contains("expected `--` before the command (got 'make')"),
        "the error must name the offending word, got:\n{stderr}"
    );
    assert!(
        stderr.contains(USAGE_LINE),
        "the usage line must accompany the error, got:\n{stderr}"
    );
}

#[test]
fn empty_command_after_separator_is_a_usage_error() {
    let world = RunWorld::new("usage-empty");

    // `gantry run --`: the separator with nothing wrapped behind it.
    let (code, _stdout, stderr) = world.run(None, &["run", "--"]);
    assert_eq!(
        code, 2,
        "empty argv after `--` must exit 2, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("gantry run: no command given"),
        "the error must say no command was given, got:\n{stderr}"
    );
    assert!(
        stderr.contains(USAGE_LINE),
        "the usage line must accompany the error, got:\n{stderr}"
    );

    // Bare `gantry run` is the same contract (no separator, no command).
    let (code, _stdout, stderr) = world.run(None, &["run"]);
    assert_eq!(code, 2, "bare `gantry run` must exit 2, stderr:\n{stderr}");
    assert!(
        stderr.contains("gantry run: no command given") && stderr.contains(USAGE_LINE),
        "bare run must carry the same error and usage line, got:\n{stderr}"
    );
}

#[test]
fn unknown_backend_value_is_a_usage_error() {
    let world = RunWorld::new("usage-backend");

    // Space spelling.
    let (code, _stdout, stderr) = world.run(None, &["run", "--backend", "bogus", "--", "true"]);
    assert_eq!(code, 2, "unknown backend must exit 2, stderr:\n{stderr}");
    assert!(
        stderr.contains("gantry run: unknown backend 'bogus'"),
        "the error must name the rejected value, got:\n{stderr}"
    );
    assert!(
        stderr.contains(USAGE_LINE),
        "the usage line must accompany the error, got:\n{stderr}"
    );

    // Equals spelling takes the same arm with the same message.
    let (code, _stdout, stderr) = world.run(None, &["run", "--backend=bogus", "--", "true"]);
    assert_eq!(
        code, 2,
        "unknown backend (=) must exit 2, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("gantry run: unknown backend 'bogus'") && stderr.contains(USAGE_LINE),
        "the equals spelling must carry the same error, got:\n{stderr}"
    );

    // And the dangling flag (a `--backend` with no value at all).
    let (code, _stdout, stderr) = world.run(None, &["run", "--backend"]);
    assert_eq!(code, 2, "dangling --backend must exit 2, stderr:\n{stderr}");
    assert!(
        stderr.contains("gantry run: --backend needs a value") && stderr.contains(USAGE_LINE),
        "the dangling flag must name itself, got:\n{stderr}"
    );
}

// ============================================================================
// The wrapped argv: program-first, verbatim, exit-code faithful.
// ============================================================================

#[test]
fn wrapped_argv_is_executed_program_first_with_the_tail_verbatim() {
    // Zero-config world: `--backend` unset means Tier-0, so the wrapped
    // command runs locally — the sharpest possible observation of the parse
    // result, because whatever was parsed IS what gets exec'd.
    let world = RunWorld::new("argv-program-first");

    // The program resolves and runs; its arguments arrive in order; its
    // stdout flows through.
    let (code, stdout, stderr) = world.run(None, &["run", "--", "echo", "gantry-run-argv-ok"]);
    assert_eq!(code, 0, "echo must succeed, stderr:\n{stderr}");
    assert!(
        stdout.contains("gantry-run-argv-ok"),
        "the wrapped command's stdout must flow through, got:\n{stdout}"
    );
    assert!(
        stderr.contains(TIER0_DECISION),
        "the zero-config run must take the Tier-0 arm, got:\n{stderr}"
    );

    // Dash-leading words after `--` belong to the wrapped tool, never to
    // gantry — a second `--` included.
    let (code, stdout, stderr) = world.run(None, &["run", "--", "echo", "--", "not-a-flag"]);
    assert_eq!(code, 0, "echo must succeed, stderr:\n{stderr}");
    assert!(
        stdout.contains("-- not-a-flag"),
        "flags after `--` must reach the wrapped command verbatim, got:\n{stdout}"
    );

    // Multi-argument tails pass through intact — proven by exit-code
    // fidelity (INV-3): `sh -c "exit 7"` only returns 7 if both the program
    // (`sh`), the flag (`-c`), and the payload landed in the right slots.
    let (code, _stdout, stderr) = world.run(None, &["run", "--", "sh", "-c", "exit 7"]);
    assert_eq!(
        code, 7,
        "the wrapped command's exit code must pass through byte-exact, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: TestFailure"),
        "a non-zero wrapped exit is the command's failure, not infra, got:\n{stderr}"
    );
}

// ============================================================================
// The `--backend` override reaches the pipeline — in both directions.
// ============================================================================

#[test]
fn command_backend_override_reaches_the_pipeline() {
    // Config says none (Tier-0 would keep the run local); the override must
    // push it through the remote pipeline, observed at the command backend:
    // the gate passes, the epoch ref is pushed, and the mock executor is
    // handed the wrapped command.
    let mut world = RunWorld::new("override-command");
    world.set_config_backend("none");
    let mock = world.mock_executor();

    let exec = mock.script.clone();
    let (code, _stdout, stderr) = world.run(
        Some(&exec),
        &[
            "run",
            "--backend",
            "command",
            "--",
            "echo",
            "via-the-pipeline",
        ],
    );

    assert_eq!(code, 0, "the pipeline run must pass, stderr:\n{stderr}");
    assert!(
        stderr.contains(REMOTE_DECISION),
        "the override must enter the remote pipeline (not Tier-0), got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the mock wait verdict must map to Pass, got:\n{stderr}"
    );

    // The pipeline actually reached submit — the decision was executed, not
    // just announced (a gate or push failure would have fallen back locally
    // and left the mock untouched).
    let record = mock.submit_record();
    assert_eq!(
        record.len(),
        3,
        "submit is handed {{repo}}, {{rev}}, {{args_json}}: {record:?}"
    );
    assert!(
        record[0].starts_with("file://") && record[0].ends_with("bare-remote.git"),
        "repo placeholder must carry the resolved origin URL, got: {}",
        record[0]
    );
    assert!(
        record[1].len() == 40 && record[1].chars().all(|c| c.is_ascii_hexdigit()),
        "rev placeholder must carry the HEAD sha, got: {}",
        record[1]
    );
    assert_eq!(
        record[2], "[\"via-the-pipeline\"]",
        "the wrapped tail must reach the backend verbatim as args_json"
    );

    // And the push the pipeline performs lands only on gantry's own refs.
    let refs = world.remote_refs();
    assert!(
        !refs.trim().is_empty(),
        "the pipeline must push the epoch ref to the CI remote"
    );
    for line in refs.lines() {
        let ref_name = line.split_whitespace().nth(1).unwrap_or("");
        assert!(
            ref_name.starts_with("refs/gantry/"),
            "only refs/gantry/* may move (INV-2), found: {ref_name}"
        );
    }
}

#[test]
fn none_override_suppresses_the_configured_command_backend() {
    // The mirror image: config opts into the command backend (the mock would
    // be reached on any remote path); `--backend none` must keep the run
    // local — Tier-0 — and leave both the mock and the remote untouched.
    let mut world = RunWorld::new("override-none");
    world.set_config_backend("command");
    let mock = world.mock_executor();

    let exec = mock.script.clone();
    let (code, stdout, stderr) = world.run(
        Some(&exec),
        &["run", "--backend", "none", "--", "echo", "stays-local"],
    );

    assert_eq!(code, 0, "the local run must succeed, stderr:\n{stderr}");
    assert!(
        stdout.contains("stays-local"),
        "the wrapped command must still run (locally), got:\n{stdout}"
    );
    assert!(
        stderr.contains(TIER0_DECISION),
        "`--backend none` must take the Tier-0 arm regardless of config, got:\n{stderr}"
    );
    assert!(
        !stderr.contains(REMOTE_DECISION),
        "the remote pipeline must not be entered, got:\n{stderr}"
    );
    assert!(
        !mock.dir.path().join("submit-argv.txt").exists(),
        "the configured command backend must never be reached"
    );
    assert!(
        world.remote_refs().trim().is_empty(),
        "nothing may be pushed to the remote, found: {}",
        world.remote_refs()
    );
}

#[test]
fn argo_backend_override_reaches_the_pipeline_decision() {
    // The argo override is proven at the decision boundary and no further:
    // driving it past the gate would exec the host kubectl, so the tree is
    // left dirty — the gate refuses, the pipeline announces the remote
    // decision, and the wrapped command comes back through the documented
    // fallback ladder with its own exit code. The decision line prints only
    // when the backend is remote, so its presence IS the override arriving.
    let world = RunWorld::new("override-argo");
    fs::write(world.project.join("dirty.txt"), "uncommitted\n").expect("dirty the tree");

    let (code, stdout, stderr) = world.run(
        None,
        &[
            "run",
            "--backend",
            "argo",
            "--",
            "echo",
            "argo-decision-only",
        ],
    );

    assert_eq!(
        code, 0,
        "the fallback must still produce the wrapped command's outcome, stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("argo-decision-only"),
        "the fallback runs the wrapped command locally, got:\n{stdout}"
    );
    assert!(
        stderr.contains(REMOTE_DECISION),
        "the argo override must enter the remote pipeline (not Tier-0), got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] ineligible:"),
        "the dirty tree must refuse the run at the git gate, got:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the documented fallback ladder must take over, got:\n{stderr}"
    );

    // The decision was recorded before the gate refused, so `gantry why`
    // replays a local run with the gate as the named reason.
    let intents: Vec<_> = world
        .runlog_records()
        .into_iter()
        .filter(|r| r["rec"] == "intent")
        .collect();
    assert_eq!(intents.len(), 1, "exactly one intent record: {intents:?}");
    assert_eq!(intents[0]["decision"], "local");
    assert!(
        intents[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("gate")),
        "the intent must name the git gate as the reason, got: {}",
        intents[0]["reason"]
    );
}
