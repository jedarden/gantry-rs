// gantry — an OOM-killed run classifies as InfraFailure, end to end, in the
// run records (gantry-0c2fbd22; parent gantry-9d3c520c, plan Component 5
// acceptance scenario 2).
//
// The classifier half of the OOM story is pinned in the shared corpus
// (tests/fixtures/failure-class-corpus.json, "oom-killed-suite-is-infra-…"):
// exit 137 is outside the classifiable window, so no failure class can ever
// dress an out-of-memory run up as tests-failed. This file closes the other
// half the acceptance names: the verdict an OOM run lands in run output and
// in runs.jsonl must be InfraFailure — never TestFailure — through the real
// binary, the real argo backend, and the real runlog write path.
//
// The argo backend needs no live cluster to be exercised honestly
// (same harness as argo_deadline_expiry_integration.rs): kubectl_path is
// configurable precisely so the backend runs against a mock executable, and
// the pipeline's kubectl calls (submit via `create -f -`, wait polls via
// `get workflow -o json`) are the whole surface a terminal-OOM path touches.
// The fake serves the workflow already terminal-Failed with a `verdict`
// output parameter carrying exactly the document the reference producer
// (contrib/argo/gantry-verify-workflowtemplate.yml) stamps for an
// out-of-memory run.
//
// Two producer shapes are real OOM and both must land InfraFailure:
// - `oom: true` — the suite was SIGKILL'd (cargo exit 137); the producer
//   stamps `emit_verdict Failed 2 "" true`: the exit-2 infra document with
//   the oom flag on top. Client-side, oom outranks the whole ladder
//   (VerdictJson::to_verdict), so even phase Failed cannot read as a test
//   failure.
// - `oom: false`, exit 2 — the pre-written infra default: the document is
//   written BEFORE anything can fail, so a container OOMKilled before it
//   could classify itself leaves this behind, and the ladder's >=2 bucket
//   owns it. A vanished suite claims nothing about the code.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// The fixture crate's only test: it never runs anywhere in this drill —
/// the fake cluster's workflow is already terminal when the first poll
/// lands, and the intercepted pipeline exits on the remote verdict instead
/// of rerunning locally.
const FIXTURE_LIB_RS: &str = "#[cfg(test)]
mod tests {
    #[test]
    fn argo_oom_fixture_test() {
        // The fake cluster answers terminal before this suite could run:
        // the OOM classification under test happens entirely client-side.
    }
}
";

/// The fake kubectl the argo backend drives. `create -f -` accepts the
/// Workflow manifest and mints a fixed workflow name (the handle); every
/// `get workflow -o json` serves the pre-built terminal workflow object
/// (Failed, with the `verdict` output parameter) that the world wrote next
/// to this script.
const FAKE_KUBECTL: &str = "#!/usr/bin/env sh
for arg in \"$@\"; do
  case \"$arg\" in
    create)
      echo 'workflow.argoproj.io/gantry-oom-e2e-0001 created'
      exit 0
      ;;
    get)
      cat __STATUS_FILE__
      exit 0
      ;;
  esac
done
echo \"fake-kubectl: unexpected invocation: $*\" >&2
exit 125
";

/// A self-contained argo-OOM world: a committed fixture git repo whose
/// `origin` is a bare remote that accepts the epoch-ref push, plus an
/// isolated HOME carrying a user-layer config that selects the argo backend
/// pointed at the fake kubectl, so an intercepted `cargo test` enters the
/// remote decision pipeline through `ArgoBackend` and reads the OOM verdict
/// off the terminal workflow.
struct ArgoOomWorld {
    /// Isolated HOME: the user-config layer, the state dir, and the runlog.
    _home: TempDir,
    /// The fixture project directory (a clean git repo, no .gantry.toml).
    project: PathBuf,
    /// The `cargo` symlink inside the project that invokes gantry.
    cargo_link: PathBuf,
    /// Backs `cargo_link`; dropped last so the symlink outlives every run.
    _realbin_dir: TempDir,
    /// Dropped last so the temp dirs outlive every Command run against them.
    _project_dir: TempDir,
}

impl ArgoOomWorld {
    /// Build the world around a `verdict_doc`: the exact verdict.json text
    /// the producer stamped, served as the workflow's `verdict` output
    /// parameter on every poll.
    fn new(name: &str, verdict_doc: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // The fake kubectl and the workflow object it serves live outside
        // the fixture repo (harness, not fixture content), referenced by
        // absolute path, so the backend's kubectl resolution never depends
        // on PATH and the served document carries no shell-escaping at all.
        let bin_dir = TempDir::new().expect("create fake-kubectl bin dir");
        let bin = bin_dir.path().to_path_buf();

        // The terminal workflow object: phase Failed, the verdict document
        // riding the `verdict` output parameter exactly as the reference
        // producer exports it (a JSON string value).
        let status_json = format!(
            r#"{{"status": {{"phase": "Failed", "outputs": {{"parameters": [{{"name": "verdict", "value": {}}}]}}}}}}"#,
            serde_json::json!(verdict_doc)
        );
        let status_file = bin.join("workflow-status.json");
        fs::write(&status_file, status_json).expect("write served workflow object");

        let fake_kubectl = bin.join("kubectl");
        fs::write(
            &fake_kubectl,
            FAKE_KUBECTL.replace(
                "__STATUS_FILE__",
                status_file.to_str().expect("status file path is UTF-8"),
            ),
        )
        .expect("write fake kubectl");
        fs::set_permissions(&fake_kubectl, fs::Permissions::from_mode(0o755))
            .expect("make fake kubectl executable");
        // Leak-free lifetime: the bin dir must outlive every gantry run, so
        // it is forgotten here (the OS reclaims /tmp's contents; the test
        // process exits long before that matters).
        std::mem::forget(bin_dir);

        // User config layer: the argo backend with the fake kubectl path.
        // The wait deadline stays at the config floor for safety only — the
        // served workflow is already terminal, so the first poll ends the
        // wait long before it could fire. (The repo layer could carry none
        // of this: backend templates and kubectl paths are user trust,
        // boundary S-2.)
        let config_dir = home.path().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("create user config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[remote]\n\
                 backend = \"argo\"\n\
                 \n\
                 [remote.argo]\n\
                 kubectl_path = \"{}\"\n\
                 deadline_minutes = 1\n",
                fake_kubectl.display()
            ),
        )
        .expect("write user config");

        // Fixture crate: trivial, dependency-free. The empty `[workspace]`
        // detaches it from any outer workspace root cargo's upward walk
        // might find (same guard as the expiry drill's fixture).
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"argo-oom-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        fs::create_dir(project.join("src")).expect("create fixture src/");
        fs::write(project.join("src/lib.rs"), FIXTURE_LIB_RS).expect("write fixture lib.rs");

        // The shim invocation: a symlink named `cargo` pointing at gantry,
        // so argv[0] dispatches to the cargo tool profile.
        let cargo_link = project.join("cargo");
        std::os::unix::fs::symlink(gantry_binary(), &cargo_link)
            .expect("create cargo→gantry symlink");

        // Bare remote that accepts every push (no hook), INSIDE the fixture
        // work tree (so everything commits and the tree gates clean).
        let bare_remote = project.join(".git-state/bare-remote.git");
        fs::create_dir_all(&bare_remote).expect("create bare remote dir");

        // Git repo + remote, with identity pinned (a global gitconfig is not
        // guaranteed under the isolated HOME).
        git(&project, &["init", "--bare", bare_remote.to_str().unwrap()]);
        git(&project, &["init"]);
        git(&project, &["config", "user.name", "Argo OOM Drill"]);
        git(
            &project,
            &["config", "user.email", "argo-oom-drill@test.invalid"],
        );
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // Pin the "real cargo" the shim's PATH lookup resolves to the cargo
        // that compiled this very test (same guard as the expiry drill: the
        // host PATH may carry a wrapper that offloads `cargo test` in a git
        // repo with an origin — and the fixture is exactly such a repo).
        let realbin_dir = TempDir::new().expect("create realbin dir");
        let realbin = realbin_dir.path().to_path_buf();
        std::os::unix::fs::symlink(env!("CARGO"), realbin.join("cargo"))
            .expect("symlink the real cargo into realbin");

        ArgoOomWorld {
            _home: home,
            project,
            cargo_link,
            _realbin_dir: realbin_dir,
            _project_dir: project_dir,
        }
    }

    fn home(&self) -> PathBuf {
        self._home.path().to_path_buf()
    }

    /// The gantry state dir under the isolated HOME.
    fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/gantry")
    }

    /// The `rec` records of the run's runs.jsonl with the given type, parsed
    /// leniently (malformed lines fail the shape check and are skipped).
    fn records(&self, rec_type: &str) -> Vec<serde_json::Value> {
        let log_path = self.state_dir().join("runs.jsonl");
        let content = fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("read runs.jsonl at {}: {e}", log_path.display()));
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|rec| rec.get("rec").and_then(|r| r.as_str()) == Some(rec_type))
            .collect()
    }

    /// The `rec: "verdict"` records of the run.
    fn verdict_records(&self) -> Vec<serde_json::Value> {
        self.records("verdict")
    }

    /// The `rec: "intent"` records of the run.
    fn intent_records(&self) -> Vec<serde_json::Value> {
        self.records("intent")
    }

    /// The flight-recorder bundles under the state dir, one directory per
    /// run id that ended in an InfraFailure.
    fn crash_dirs(&self) -> Vec<PathBuf> {
        let crash_root = self.state_dir().join("crash");
        let mut dirs: Vec<PathBuf> = fs::read_dir(&crash_root)
            .unwrap_or_else(|e| panic!("read crash dir at {}: {e}", crash_root.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        dirs.sort();
        dirs
    }

    /// The raw runs.jsonl text, for no-test-failure-anywhere assertions.
    fn raw_runs_jsonl(&self) -> String {
        let log_path = self.state_dir().join("runs.jsonl");
        fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("read runs.jsonl at {}: {e}", log_path.display()))
    }

    /// Run the shim as `cargo test` in the fixture. Returns (exit, stdout,
    /// stderr).
    fn run_shim(&self) -> (i32, String, String) {
        let mut cmd = Command::new(&self.cargo_link);
        cmd.current_dir(&self.project).args(["test"]);
        // The shim must resolve the REAL cargo (never itself) through a PATH
        // that pins realbin first.
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                self._realbin_dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        self.isolate(&mut cmd);
        let output = cmd.output().expect("run cargo shim");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// Isolation env for gantry children: throwaway HOME, no XDG redirects,
    /// no inherited GANTRY_* knobs, no shared target dir.
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
        cmd.env_remove("CARGO_TARGET_DIR");
        cmd.env_remove("CARGO_BUILD_TARGET_DIR");
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The OOM classification pin, shared by both producer shapes: the run went
/// through the argo backend, its terminal InfraFailure verdict is what the
/// run output and the runlog record both carry — never a test failure — the
/// exit code is the infra ladder's 2 (never the test-failure 1), the flight
/// recorder holds the attempt's bundle, and no test_failure reading exists
/// anywhere in the ledger.
fn assert_oom_run_lands_infra_failure(world: &ArgoOomWorld, stderr: &str, exit_code: i32) {
    // The run went through the argo backend, not the command templates: the
    // handle is the workflow name the fake kubectl minted at `create`.
    assert!(
        stderr.contains("[gantry] submitted: gantry-oom-e2e-0001"),
        "the submit must have run through the argo backend (the fake \
         kubectl's workflow name), got stderr:\n{stderr}"
    );

    // The verdict the caller saw is infra — and nothing on the path dressed
    // the OOM up as a test failure or engaged the local ladder (the
    // intercepted pipeline exits on the terminal verdict it earned).
    assert!(
        stderr.contains("[gantry] verdict: InfraFailure"),
        "an OOM run must classify as InfraFailure, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("verdict: TestFailure"),
        "an OOM run must never classify as a test failure, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("falling back to capped local run"),
        "a terminal remote verdict is the answer — no local rerun may \
         overwrite it, got stderr:\n{stderr}"
    );
    assert_eq!(
        exit_code, 2,
        "the InfraFailure ladder code is 2 — never the test-failure 1, \
         got stderr:\n{stderr}"
    );

    // The intent names the backend that ran.
    let intents = world.intent_records();
    assert_eq!(intents.len(), 1, "exactly one intent record");
    assert_eq!(
        intents[0]["backend"], "argo",
        "the intent must name the argo backend"
    );

    // The ledger carries exactly one terminal record: the remote attempt's
    // InfraFailure — never a test failure under any spelling.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "infra_failure",
        "the OOM run's record is InfraFailure, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["ran"], "remote",
        "the record is the remote attempt's own"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 2,
        "the recorded exit code is the infra ladder's, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["handle"], "gantry-oom-e2e-0001",
        "the record names the run that was watched"
    );
    assert!(
        verdicts[0].get("timeout").is_none(),
        "no deadline expired here — the timeout field must be absent, got: {verdicts:?}"
    );
    assert!(
        verdicts[0].get("failure_class").is_none(),
        "an OOM run has no failure class — the classifiable window leaves it \
         unset, got: {verdicts:?}"
    );
    assert!(
        !world.raw_runs_jsonl().contains("test_failure"),
        "no record in the ledger may read as a test failure, got:\n{}",
        world.raw_runs_jsonl()
    );

    // The remote attempt is flight-recorded: the InfraFailure bundle is on
    // disk (the remote-verdict stage writes no backend-response artifact —
    // wait() returned cleanly — but the config snapshot is unconditional).
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the OOM attempt is flight-recorded exactly once, got: {crash_dirs:?}"
    );
    assert!(
        crash_dirs[0].join("config.json").exists(),
        "the bundle carries the config snapshot, got: {:?}",
        crash_dirs[0]
    );
}

/// The producer's SIGKILL stamp (gantry-verify: `emit_verdict Failed 2 ""
/// true` for cargo exit 137): the suite was OOM-killed mid-run and the
/// producer flagged it so. The client must honor the oom signal over the
/// Failed phase — InfraFailure in the run output and in runs.jsonl, never a
/// test failure.
#[test]
fn oom_killed_run_with_the_producer_oom_stamp_classifies_as_infra_failure() {
    let verdict_doc = r#"{"schema_version": 1, "phase": "Failed", "exit_code": 2, "oom": true, "deadline_exceeded": false, "contract_version": "1"}"#;
    let world = ArgoOomWorld::new("oom-stamp", verdict_doc);
    let (exit_code, _stdout, stderr) = world.run_shim();
    assert_oom_run_lands_infra_failure(&world, &stderr, exit_code);
}

/// The pre-written infra default (gantry-verify writes the exit-2 document
/// BEFORE anything can fail): a container OOMKilled before it could
/// classify itself leaves this behind — no oom flag, no class, phase Failed.
/// The ladder's >=2 bucket still owns it: a vanished suite claims nothing
/// about the code, so the run is InfraFailure, never tests-failed.
#[test]
fn oomkilled_container_leaving_the_pre_written_infra_default_classifies_as_infra_failure() {
    let verdict_doc = r#"{"schema_version": 1, "phase": "Failed", "exit_code": 2, "oom": false, "deadline_exceeded": false, "contract_version": "1"}"#;
    let world = ArgoOomWorld::new("oom-default", verdict_doc);
    let (exit_code, _stdout, stderr) = world.run_shim();
    assert_oom_run_lands_infra_failure(&world, &stderr, exit_code);
}
