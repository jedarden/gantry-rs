// gantry — the argo wait expiry, end to end, to the capped-local fallback
// (gantry-c21e4585; parent gantry-db477308, plan DD-4 + Component 6).
//
// The earlier links in this chain are each pinned one stage deep: the
// per-backend deadline config (gantry-86a27828), the expiry's
// InfraFailure classification with no fabricated verdict
// (gantry-de0d096d), and the fallback routing of the expiry-derived
// InfraFailure (gantry-fcacf4de, whose `deadline_expiry_integration.rs`
// drives the *command* backend because its wait templates are callable
// without a cluster). This test closes the loop the parent demands: the
// real binary, the real argo backend, a wait that actually hits its
// deadline, and the capped-local ladder that picks the run up.
//
// The argo backend needs no live cluster to be exercised honestly:
// `ArgoConfig.kubectl_path` is configurable precisely so the backend runs
// against a mock executable, and the pipeline's kubectl calls (submit via
// `create -f -`, wait polls via `get workflow -o json`) are the whole
// surface the expiry path touches. The fake here accepts the submit and
// serves a forever-pending workflow — the controller never reconciles, so
// the wait loop can only end at its deadline, exactly the expiry the
// classification and routing tails exist for.
//
// Exercising this path end to end exposed one piece of missing glue, fixed
// alongside the test: both decision pipelines constructed the command
// backend unconditionally, so a configured `backend = "argo"` never reached
// `ArgoBackend` at all (it silently ran the command templates). The
// pipelines now dispatch on the configured backend
// (`decision::build_backend`), and the intent record names the backend that
// actually runs — both pinned here.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// The fixture crate's only test: the remote wait never returns a verdict
/// for it — the capped-local fallback is what runs it, and its pass is the
/// run's earned verdict.
const FIXTURE_LIB_RS: &str = "#[cfg(test)]
mod tests {
    #[test]
    fn argo_expiry_fixture_test() {
        // The remote attempt never gets here: the wait deadline fires first.
        // The capped-local fallback reruns this suite for real.
    }
}
";

/// The fake kubectl the argo backend drives. `create -f -` accepts the
/// Workflow manifest and mints a fixed workflow name (the handle); every
/// `get workflow -o json` reports the workflow still pending (`status`
/// present, `phase` absent — the shape the backend's phase ladder treats as
/// "the controller has not reconciled yet"). The deadline is therefore the
/// only thing that can end the wait.
const FAKE_KUBECTL: &str = "#!/usr/bin/env sh
for arg in \"$@\"; do
  case \"$arg\" in
    create)
      echo 'workflow.argoproj.io/gantry-expiry-e2e-0001 created'
      exit 0
      ;;
    get)
      printf '{\"status\":{}}'
      exit 0
      ;;
  esac
done
echo \"fake-kubectl: unexpected invocation: $*\" >&2
exit 125
";

/// A self-contained argo-expiry world: a committed fixture git repo whose
/// `origin` is a bare remote that accepts the epoch-ref push, plus an
/// isolated HOME carrying a user-layer config that selects the argo backend
/// pointed at the fake kubectl, so an intercepted `cargo test` enters the
/// remote decision pipeline through `ArgoBackend` and dies in its wait.
struct ArgoWorld {
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

impl ArgoWorld {
    /// Build the world. The per-backend `remote.argo.deadline_minutes`
    /// override (config floor: 1) is what the wait deadline resolves
    /// through — the gantry-86a27828 wiring this chain depends on.
    fn new(name: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // The fake kubectl lives outside the fixture repo (it is harness,
        // not fixture content) and is referenced by absolute path, so the
        // backend's kubectl resolution never depends on PATH.
        let bin_dir = TempDir::new().expect("create fake-kubectl bin dir");
        let fake_kubectl = bin_dir.path().join("kubectl");
        fs::write(&fake_kubectl, FAKE_KUBECTL).expect("write fake kubectl");
        fs::set_permissions(&fake_kubectl, fs::Permissions::from_mode(0o755))
            .expect("make fake kubectl executable");
        // Leak-free lifetime: the bin dir must outlive every gantry run, so
        // it is forgotten here (the OS reclaims /tmp's contents; the test
        // process exits long before that matters).
        std::mem::forget(bin_dir);

        // User config layer: the argo backend with the fake kubectl path,
        // and the per-backend wait deadline at the config floor. (The repo
        // layer could carry none of this: `backend` templates and kubectl
        // paths are user trust, boundary S-2.)
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
        // detaches it from any outer workspace root cargo's upward walk might
        // find (same guard as the Tier-0 fixture).
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"argo-expiry-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        fs::create_dir(project.join("src")).expect("create fixture src/");
        fs::write(project.join("src/lib.rs"), FIXTURE_LIB_RS).expect("write fixture lib.rs");

        // The shim invocation: a symlink named `cargo` pointing at gantry, so
        // argv[0] dispatches to the cargo tool profile.
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
        git(&project, &["config", "user.name", "Argo Expiry Drill"]);
        git(
            &project,
            &["config", "user.email", "argo-expiry-drill@test.invalid"],
        );
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // Pin the "real cargo" the shim's PATH lookup resolves to the cargo
        // that compiled this very test (same guard as the deadline drill:
        // the host PATH may carry a wrapper that offloads `cargo test` in a
        // git repo with an origin — and the fixture is exactly such a repo).
        let realbin_dir = TempDir::new().expect("create realbin dir");
        let realbin = realbin_dir.path().to_path_buf();
        std::os::unix::fs::symlink(env!("CARGO"), realbin.join("cargo"))
            .expect("symlink the real cargo into realbin");

        ArgoWorld {
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

/// The argo wait deadline fires and the run lands a real result through the
/// capped-local fallback (DD-4 + plan Component 6, end to end through the
/// argo backend): the submitted workflow is minted by the fake kubectl, the
/// forever-pending workflow outlives its configured per-backend deadline, the
/// expiry surfaces through the tail as the `[gantry] timeout:` line carrying
/// the abandoned run's identifier, and the remote attempt is flight-recorded
/// as the InfraFailure it is — never terminated on a bare infra exit, never
/// given a verdict it did not earn. The capped-local ladder then reruns the
/// fixture suite for real, and its own outcome becomes the verdict: exit 0,
/// `verdict: pass`, `ran: local_after_infra`, exit code the local rerun's
/// own — exactly one terminal record, carrying the expiry as its timeout
/// detail (backend, handle, reason) so the ledger names what the local pass
/// is the fallback for, and nothing fabricated anywhere on the path.
#[test]
fn argo_wait_deadline_expiry_falls_back_to_the_capped_local_ladder() {
    let world = ArgoWorld::new("expiry-e2e");
    let (exit_code, _stdout, stderr) = world.run_shim();

    // The run went through the argo backend, not the command templates: the
    // handle is the workflow name the fake kubectl minted at `create`.
    assert!(
        stderr.contains("[gantry] submitted: gantry-expiry-e2e-0001"),
        "the submit must have run through the argo backend (the fake \
         kubectl's workflow name), got stderr:\n{stderr}"
    );
    // The intent record names the backend that actually ran.
    let intents = world.intent_records();
    assert_eq!(
        intents.len(),
        1,
        "exactly one intent record, got: {intents:?}"
    );
    assert_eq!(
        intents[0]["backend"], "argo",
        "the intent must name the argo backend, not a hardcoded one"
    );

    // The wait hit its deadline — the timeout line naming the run, with the
    // abandoned run's identifier for the operator (the argo backend's
    // describe(): no base_url configured, so the bare identifier form).
    assert!(
        stderr.contains(
            "[gantry] timeout: run gantry-expiry-e2e-0001 exceeded its deadline \
             before a verdict was returned"
        ),
        "the timeout line naming the run must report the expiry, got stderr:\n{stderr}"
    );
    assert!(
        stderr
            .contains("the abandoned run can still be watched at workflow/gantry-expiry-e2e-0001"),
        "the expiry must carry the abandoned run's identifier, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] wait failed:"),
        "an expiry must not wear the generic wait-failure line, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] verdict: InfraFailure"),
        "the expiry must not terminate as a bare infra exit — the ladder's \
         local outcome is the verdict, got stderr:\n{stderr}"
    );

    // The fallback engaged, carrying the expiry as its infra reason.
    assert!(
        stderr.contains("[gantry] infra: ")
            && stderr.contains("deadline exceeded while polling status.phase"),
        "the fallback must carry the expiry as its infra reason, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the expiry must engage the capped-local fallback ladder, got stderr:\n{stderr}"
    );

    // The ladder's local rerun ran the fixture suite for real and its own
    // outcome is the answer.
    assert_eq!(
        exit_code, 0,
        "the capped local rerun's own passing exit is the answer — a real \
         result, not a fabricated one, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the trailer must report the local rerun's earned verdict, got stderr:\n{stderr}"
    );

    // The remote attempt's classification lives in the flight recorder, not
    // in a fabricated bare exit: the wait stage's bundle is on disk.
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the expired attempt is flight-recorded exactly once, got: {crash_dirs:?}"
    );
    assert!(
        crash_dirs[0].join("backend-wait.txt").exists(),
        "the wait stage's raw backend response rides the bundle, got: {:?}",
        crash_dirs[0]
    );

    // No verdict was fabricated anywhere on the path: the only terminal
    // record is the local rerun's own outcome.
    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "pass",
        "the ledger records the local rerun's earned verdict"
    );
    assert_eq!(
        verdicts[0]["ran"], "local_after_infra",
        "the terminal record is the fallback's, not the remote attempt's"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 0,
        "the recorded exit code is the local rerun's own, never an invented one"
    );

    // The expiry is not lost to the local rerun's outcome: the terminal
    // record carries the timeout detail — the backend whose watch expired,
    // the abandoned run's handle, and the expiry reason verbatim (the same
    // three facts the `[gantry] timeout` line printed) — so runs.jsonl
    // identifies the deadline timeout behind the local pass.
    assert_eq!(
        verdicts[0]["timeout"]["backend"], "argo",
        "the terminal record must name the backend whose watch expired, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["timeout"]["handle"], "gantry-expiry-e2e-0001",
        "the terminal record must carry the abandoned run's handle, got: {verdicts:?}"
    );
    assert!(
        verdicts[0]["timeout"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("deadline exceeded while polling status.phase")),
        "the terminal record must carry the backend's expiry reason, got: {verdicts:?}"
    );
}
