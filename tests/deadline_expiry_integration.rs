// gantry — deadline-expiry classification and fallback routing through the
// decision wait tails (gantry-de0d096d classification, gantry-fcacf4de
// routing; plan DD-4, Component 6).
//
// The decision pipeline's wait-failure tails (src/decision.rs) are the place
// a deadline-expired remote attempt becomes a verdict: DD-4 says the expiry
// must classify as InfraFailure and must never fabricate one — no defaulted
// success, no invented passing exit code — and plan Component 6 routes that
// expiry through the capped-local fallback ladder instead of ending the run
// on a bare infra exit. The wait boundary itself (the backend raising the
// structured deadline error, distinguishable from a generic wait failure) is
// pinned in src/backend/argo.rs; these tests drive the real binary past that
// boundary and pin what the tails do with it.
//
// Both wait tails are pinned here: the intercepted pipeline (`run_remote`,
// entered through the cargo shim) and the explicit-offload pipeline
// (`run_explicit`, entered through `gantry run` invoked directly). The tails
// classify an expiry identically — timeout line, flight-recorder
// InfraFailure, capped-local ladder — but they do not route their generic
// wait failures identically (the intercepted tail keeps the bare infra exit,
// the offload tail degrades on every wait failure), so each tail's contrast
// case is pinned on its own.
//
// The expiry also has a second shape the tails must classify identically: a
// terminal `Ok(Verdict::InfraFailure)` — what a remote verdict.json carrying
// the deadline_exceeded infra signal produces, no Err raised at the wait
// boundary at all. Those arms are pinned here too (gantry-957eb9f5): the
// remote-verdict bundle is written and the caller never observes a passing
// exit from the terminal verdict itself — on the intercepted tail the
// verdict's own to_exit_code (2) is the answer; on the offload tail the
// ladder's local rerun runs for real, and the drill's fixture suite fails so
// a fabricated passing exit would have nothing to hide behind. The offload
// tail's terminal pin enters through the library entry the `run` dispatch
// calls ([`gantry::cli::run::cli`]), driven in-process — the pin must hold at
// every committed state, including the ones where the dispatch line in
// main.rs has not landed yet (gantry-51de1af3); the binary-level `gantry
// run` spelling is what the Err-shape offload pins above exercise.
//
// The command backend is the in-test producer of the identical structured
// error (the argo backend needs a live cluster to watch): `wait()` kills a
// wait command that outlives the configured deadline and returns
// `BackendError::deadline`, exactly the shape the tails consume. The config
// minimum deadline is one minute, so the expiry test pays that minute; the
// contrast test fails the wait at spawn time and is fast.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// The fixture crate's only test: never runs (the remote wait never returns
/// a verdict for it), but keeps the fixture a plausible cargo project.
const FIXTURE_LIB_RS: &str = "#[cfg(test)]
mod tests {
    #[test]
    fn wait_fixture_test() {
        // The remote pipeline never gets here: the wait fails first.
    }
}
";

/// The failing fixture's one test: panics, so the capped-local ladder's
/// rerun of it earns a non-passing exit — the terminal-InfraFailure offload
/// drill needs a local outcome a fabricated pass cannot hide behind.
const FAILING_FIXTURE_LIB_RS: &str = "#[cfg(test)]
mod tests {
    #[test]
    fn failing_fixture_test() {
        panic!(\"the fixture suite fails, by design\");
    }
}
";

/// A self-contained wait-failure world: a committed fixture git repo whose
/// `origin` is a bare remote that accepts the epoch-ref push, plus an
/// isolated HOME carrying a user-layer config that opts into the command
/// backend with caller-controlled templates (user layer — the repo layer
/// cannot set them, trust boundary S-2), so an intercepted `cargo test`
/// enters the remote decision pipeline (run_remote) and dies in its wait —
/// or a direct `gantry run -- cargo test` enters the explicit-offload
/// pipeline (run_explicit) and dies in the same wait.
struct WaitWorld {
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

impl WaitWorld {
    /// Build the world. `wait_template` is the `[remote.command]` wait argv
    /// under test; `deadline_minutes` is the command backend's enforced wait
    /// deadline (config floor: 1).
    fn new(name: &str, wait_template: &[&str], deadline_minutes: u64) -> Self {
        Self::build(name, wait_template, deadline_minutes, FIXTURE_LIB_RS)
    }

    /// The same world with a fixture whose suite fails when it runs. The
    /// fixture must fail *before* the world commits it — a post-commit edit
    /// would leave the tree dirty and the git gate would shunt the run onto
    /// the gate-ineligible local tail, never reaching the wait under test.
    fn new_failing(name: &str, wait_template: &[&str], deadline_minutes: u64) -> Self {
        Self::build(
            name,
            wait_template,
            deadline_minutes,
            FAILING_FIXTURE_LIB_RS,
        )
    }

    fn build(name: &str, wait_template: &[&str], deadline_minutes: u64, fixture_lib: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // User config layer: command backend with explicit templates. Submit
        // prints a fixed handle; logs is a no-op; wait is the caller's argv.
        let handle = format!("handle-{name}");
        let config_dir = home.path().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("create user config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[remote]\n\
                 backend = \"command\"\n\
                 \n\
                 [remote.command]\n\
                 submit = [\"echo\", \"{handle}\"]\n\
                 logs = [\"true\"]\n\
                 wait = [{}]\n\
                 deadline_minutes = {deadline_minutes}\n",
                wait_template
                    .iter()
                    .map(|arg| format!("\"{arg}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        )
        .expect("write user config");

        // Fixture crate: trivial, dependency-free. The empty `[workspace]`
        // detaches it from any outer workspace root cargo's upward walk might
        // find (same guard as the Tier-0 fixture).
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"wait-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            ),
        )
        .expect("write fixture Cargo.toml");
        fs::create_dir(project.join("src")).expect("create fixture src/");
        fs::write(project.join("src/lib.rs"), fixture_lib).expect("write fixture lib.rs");

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
        git(&project, &["config", "user.name", "Wait Drill"]);
        git(
            &project,
            &["config", "user.email", "wait-drill@test.invalid"],
        );
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // Pin the "real cargo" the shim's PATH lookup resolves to the cargo
        // that compiled this very test (same guard as the crash drill: the
        // host PATH may carry a wrapper that offloads `cargo test` in a git
        // repo with an origin — and the fixture is exactly such a repo).
        let realbin_dir = TempDir::new().expect("create realbin dir");
        let realbin = realbin_dir.path().to_path_buf();
        std::os::unix::fs::symlink(env!("CARGO"), realbin.join("cargo"))
            .expect("symlink the real cargo into realbin");

        WaitWorld {
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

    /// The `rec: "verdict"` records of the run's runs.jsonl, parsed leniently
    /// (event and intent lines fail the shape check and are skipped).
    fn verdict_records(&self) -> Vec<serde_json::Value> {
        let log_path = self.state_dir().join("runs.jsonl");
        let content = fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("read runs.jsonl at {}: {e}", log_path.display()));
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|rec| rec.get("rec").and_then(|r| r.as_str()) == Some("verdict"))
            .collect()
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

    /// Run the binary as an explicit offload in the fixture: `gantry run --
    /// cargo test`, the same wrapped command the shim driver runs, entered at
    /// run_explicit directly instead of through the cargo shim. Same env
    /// contract as `run_shim`: realbin first so the capped-local ladder
    /// resolves the real cargo, isolated HOME, no inherited knobs.
    fn run_offload(&self) -> (i32, String, String) {
        let mut cmd = Command::new(gantry_binary());
        cmd.current_dir(&self.project)
            .args(["run", "--", "cargo", "test"]);
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                self._realbin_dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        self.isolate(&mut cmd);
        let output = cmd.output().expect("run gantry run offload");
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

/// The same, returning trimmed stdout — for the repo-identity resolution the
/// `run` dispatch does before entering the library entry.
fn git_out(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Process state for the in-process offload entry, restored on drop: the
/// library call runs against the current process, so the test stands in for
/// the binary dispatch's environment — cwd = the fixture project, HOME = the
/// isolated home, PATH = realbin first, and the same knobs `isolate` strips
/// from spawned children (XDG redirects, shared target dirs, GANTRY_*).
/// Restoring in `Drop` matters: a failed assert mid-pin must not strand the
/// process in the fixture world for the tests still to run.
struct DispatchEnv {
    prev_cwd: PathBuf,
    prev_vars: Vec<(String, Option<std::ffi::OsString>)>,
}

impl DispatchEnv {
    fn enter(project: &Path, home: &Path, realbin: &Path) -> Self {
        let mut prev_vars: Vec<(String, Option<std::ffi::OsString>)> = Vec::new();
        // HOME: the isolated home — Config::load, the state dir, and the
        // runlog all resolve through it.
        prev_vars.push(("HOME".into(), std::env::var_os("HOME")));
        std::env::set_var("HOME", home);
        // PATH: realbin first, the same contract `run_shim`/`run_offload`
        // give their spawned binary, so the ladder's cargo and the wait's
        // sh resolve exactly as they do there.
        prev_vars.push(("PATH".into(), std::env::var_os("PATH")));
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                realbin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        // The knobs `isolate` strips from spawned children, stripped from the
        // process the library call runs in.
        let stripped: Vec<String> = std::env::vars_os()
            .map(|(key, _)| key.to_string_lossy().to_string())
            .filter(|key| {
                key.starts_with("GANTRY_")
                    || key == "XDG_CONFIG_HOME"
                    || key == "XDG_STATE_HOME"
                    || key == "CARGO_TARGET_DIR"
                    || key == "CARGO_BUILD_TARGET_DIR"
            })
            .collect();
        for key in stripped {
            prev_vars.push((key.clone(), std::env::var_os(&key)));
            std::env::remove_var(&key);
        }
        let prev_cwd = std::env::current_dir().expect("read the current directory");
        std::env::set_current_dir(project).expect("enter the fixture project");
        DispatchEnv {
            prev_cwd,
            prev_vars,
        }
    }
}

impl Drop for DispatchEnv {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev_cwd);
        for (key, value) in &self.prev_vars {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A deadline-expired attempt classifies as InfraFailure and degrades through
/// the capped-local fallback ladder (DD-4 + plan Component 6, gantry-fcacf4de):
/// the wait that outlives its configured deadline surfaces through the tail as
/// the `[gantry] timeout:` line naming the run, the remote attempt is
/// flight-recorded as the InfraFailure it is, and the run falls back to the
/// capped local ladder instead of terminating on a bare infra exit — where the
/// fixture suite runs for real and its own outcome becomes the verdict. A
/// locally earned pass is not a fabricated one; the ledger's one terminal
/// record is that local outcome, `ran: local_after_infra`.
#[test]
fn deadline_expired_attempt_falls_back_to_the_capped_local_ladder() {
    // The wait command never returns: `sleep 120` outlives the one-minute
    // config floor, so the deadline — not the wait command — is what ends
    // the wait.
    let world = WaitWorld::new("expiry", &["sleep", "120"], 1);
    let (exit_code, _stdout, stderr) = world.run_shim();

    assert!(
        stderr.contains(
            "[gantry] timeout: run handle-expiry exceeded its deadline before a verdict was returned"
        ),
        "the timeout line naming the run must report the expiry, got stderr:\n{stderr}"
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
    assert!(
        stderr.contains("[gantry] infra: ") && stderr.contains("deadline exceeded"),
        "the fallback must carry the expiry as its infra reason, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the expiry must engage the capped-local fallback ladder, got stderr:\n{stderr}"
    );
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
}

/// A non-expiry wait failure keeps the tail's existing classification — the
/// same InfraFailure verdict, record, and infra exit, via the generic
/// `[gantry] wait failed:` line rather than the timeout line, and without the
/// fallback the expiry takes: the degrade-to-local is the expiry's routing
/// (plan Component 6), not a general wait-failure policy. The expiry and the
/// generic failure stay tellable apart all the way through the tail; neither
/// one leaks into the other's classification or routing.
#[test]
fn generic_wait_failure_keeps_the_infrafailure_classification() {
    // The wait command does not exist: the spawn fails before any deadline
    // could matter, so this is a generic wait failure, not an expiry.
    let world = WaitWorld::new("generic", &["gantry-no-such-wait-cmd"], 15);
    let (exit_code, _stdout, stderr) = world.run_shim();

    assert_eq!(
        exit_code, 1,
        "a generic wait failure is an infra exit, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] wait failed:") && stderr.contains("gantry-no-such-wait-cmd"),
        "the generic wait-failure line must carry the backend's reason, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] timeout:"),
        "a non-expiry wait failure must not wear the timeout line, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] falling back to capped local run"),
        "the fallback ladder is the expiry's routing alone — a generic wait \
         failure keeps the bare tail, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: InfraFailure"),
        "the trailer must keep the InfraFailure classification, got stderr:\n{stderr}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "infra_failure",
        "the existing classification of a generic wait failure is InfraFailure"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 1,
        "the recorded exit code must be the infra exit"
    );
    assert!(
        verdicts[0].get("timeout").is_none(),
        "a non-expiry failure must not carry the timeout detail, got: {verdicts:?}"
    );
}

/// The explicit-offload tail (`gantry run`, run_explicit) under a
/// deadline-expired wait (DD-4, plan Component 6): the expiry is reported on
/// the timeout line, classified InfraFailure in the flight recorder — the Err
/// arm writes no verdict record at all, so nothing fabricated goes in the
/// ledger — and the run degrades through the capped-local ladder, whose
/// locally earned outcome and its own exit code are what the caller gets. No
/// fabricated verdict, no invented passing exit code: the ledger's one
/// terminal record is the ladder's, `ran: local_after_infra`.
///
/// The offload tail's fallback context names no timeout (`resolve_and_fall_back`
/// passes `timeout: None` on every rung): unlike the intercepted tail — whose
/// expiry the first test above sees stamped onto the terminal record — the
/// expiry's classification home on this tail is the timeout line and the
/// flight-recorder bundle. That absence is pinned, so stamping it later is a
/// deliberate decision rather than a drift.
#[test]
fn run_explicit_deadline_expired_wait_lands_a_real_result_without_a_fabricated_verdict() {
    // Same shape as the intercepted tail's expiry drill: a wait command that
    // never returns, so the one-minute config floor's deadline — not the wait
    // command — is what ends the wait.
    let world = WaitWorld::new("offload-expiry", &["sleep", "120"], 1);
    let (exit_code, _stdout, stderr) = world.run_offload();

    assert!(
        stderr.contains(
            "[gantry] timeout: run handle-offload-expiry exceeded its deadline before a verdict was returned"
        ),
        "the timeout line naming the run must report the expiry, got stderr:\n{stderr}"
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
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the expiry must engage the capped-local fallback ladder, got stderr:\n{stderr}"
    );
    assert_eq!(
        exit_code, 0,
        "the capped local rerun's own passing exit is the answer — a real \
         result, not an invented one, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the trailer must report the local rerun's earned verdict, got stderr:\n{stderr}"
    );

    // The remote attempt's classification lives in the flight recorder, not
    // in a fabricated verdict: the wait stage's bundle is on disk, exactly
    // once.
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
        "the terminal record is the fallback's, never a remote verdict for the \
         abandoned attempt"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 0,
        "the recorded exit code is the local rerun's own, never an invented one"
    );
    assert!(
        verdicts[0].get("timeout").is_none(),
        "the offload tail stamps no timeout detail on the ledger — its expiry \
         home is the timeout line and the flight recorder, got: {verdicts:?}"
    );
}

/// Contrast, same tail: a non-expiry wait failure keeps the offload tail's
/// existing classification — the generic `[gantry] wait failed:` line, the
/// InfraFailure flight record, and the capped-local ladder. Unlike the
/// intercepted tail, whose generic failures keep the bare infra exit, the
/// offload tail's Err arm does not branch on the expiry flag: every wait
/// failure degrades. What the pins hold fixed is that the generic failure
/// keeps ITS shape — generic line rather than the timeout line, flight-
/// recorded, real local outcome — so the two failure modes stay tellable
/// apart on this tail all the way through, neither leaking into the other's
/// classification.
#[test]
fn run_explicit_generic_wait_failure_keeps_its_existing_classification() {
    // The wait command does not exist: the spawn fails before any deadline
    // could matter, so this is a generic wait failure, not an expiry.
    let world = WaitWorld::new("offload-generic", &["gantry-no-such-wait-cmd"], 15);
    let (exit_code, _stdout, stderr) = world.run_offload();

    assert!(
        stderr.contains("[gantry] wait failed:") && stderr.contains("gantry-no-such-wait-cmd"),
        "the generic wait-failure line must carry the backend's reason, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] timeout:"),
        "a non-expiry wait failure must not wear the timeout line, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] falling back to capped local run"),
        "the offload tail's existing routing for every wait failure is the \
         capped-local ladder, got stderr:\n{stderr}"
    );
    assert_eq!(
        exit_code, 0,
        "the ladder's real local outcome is the answer, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "the trailer must report the local rerun's earned verdict, got stderr:\n{stderr}"
    );

    // The classification is kept: the attempt is flight-recorded as the
    // InfraFailure it is.
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the failed attempt is flight-recorded exactly once, got: {crash_dirs:?}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["ran"], "local_after_infra",
        "the terminal record is the fallback's local outcome"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 0,
        "the recorded exit code is the local rerun's own"
    );
    assert!(
        verdicts[0].get("timeout").is_none(),
        "a non-expiry failure must not carry the timeout detail, got: {verdicts:?}"
    );
}

/// A terminal InfraFailure verdict on the intercepted tail — wait() returning
/// `Ok(Verdict::InfraFailure)`, the shape a remote verdict.json carrying the
/// deadline_exceeded infra signal produces with no Err raised at the wait
/// boundary (DD-4: "OOMKilled and deadline-exceeded are InfraFailure by
/// definition") — ends the run on the verdict's own faithful exit code and
/// never a fabricated passing one (gantry-957eb9f5). The command backend
/// produces the same Ok verdict by exiting 2 (the ladder's ≥2 → InfraFailure
/// mapping): the tail flight-records the remote-verdict bundle, writes the
/// ledger's one terminal record from that verdict, and returns its
/// `to_exit_code()` — 2, the bare infra exit this tail keeps, with no
/// capped-local detour.
#[test]
fn terminal_infrafailure_verdict_keeps_the_faithful_infra_exit() {
    // The wait command exits 2 immediately: the deadline never matters, the
    // verdict is a clean Ok(InfraFailure).
    let world = WaitWorld::new("terminal-infra", &["sh", "-c", "exit 2"], 15);
    let (exit_code, _stdout, stderr) = world.run_shim();

    assert_eq!(
        exit_code, 2,
        "the caller's exit is the verdict's own to_exit_code — the faithful \
         non-passing infra code, never a defaulted success, got stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[gantry] verdict: InfraFailure"),
        "the trailer must keep the InfraFailure classification, got stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("[gantry] falling back to capped local run"),
        "the intercepted tail keeps the bare infra exit on a terminal \
         verdict — no fallback, got stderr:\n{stderr}"
    );

    // The remote attempt's classification home: the remote-verdict bundle,
    // written even though wait() itself returned cleanly.
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the terminal verdict is flight-recorded exactly once, got: {crash_dirs:?}"
    );
    let events = fs::read_to_string(crash_dirs[0].join("events.jsonl"))
        .expect("read the bundle's events.jsonl");
    assert!(
        events.contains("\"stage\":\"remote-verdict\""),
        "the bundle names the remote-verdict stage, got:\n{events}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "infra_failure",
        "the ledger records the remote verdict's own classification"
    );
    assert_eq!(
        verdicts[0]["ran"], "remote",
        "the terminal record is the remote verdict's, not a local rerun's"
    );
    assert_eq!(
        verdicts[0]["exit_code"], 2,
        "the recorded exit code is the same faithful infra code the caller got"
    );
}

/// The same terminal verdict on the offload tail (`run_explicit`): the
/// remote-verdict bundle is written, then the run degrades through
/// `resolve_and_fall_back` — and because the drill's fixture suite FAILS, the
/// ladder's locally earned outcome is non-passing too. That is what makes the
/// no-fabrication pin two-sided: the caller's exit is the local rerun's own
/// failing code, so any defaulted success or synthesized passing exit on this
/// path would stand out against a fixture that genuinely failed (INV-3: the
/// wrapped command's exit code wins).
///
/// The pin enters through the library entry the `run` dispatch calls
/// ([`gantry::cli::run::cli`]), driven in-process with the dispatch's own
/// three lines stood in for: the repo identity resolved exactly as
/// `get_repo_url`/`get_current_sha` resolve it, the cwd moved to the fixture
/// project, and the isolated env ([`DispatchEnv`], restored on drop). The
/// pin must hold at every committed state — including the ones where the
/// dispatch line in main.rs has not landed yet (gantry-51de1af3) — so it
/// cannot wait on the binary-level `gantry run` spelling. In-process,
/// libtest captures the trailer lines, so the degrade is pinned by its
/// observable state instead: the remote-verdict bundle on disk, the ladder's
/// ledger record, and the local rerun's earned failing exit.
#[test]
fn run_explicit_terminal_infrafailure_verdict_lands_a_real_non_passing_exit() {
    let world = WaitWorld::new_failing("offload-terminal-infra", &["sh", "-c", "exit 2"], 15);

    // The dispatch's repo identity, resolved the way main.rs resolves it:
    // the origin URL, local paths spelled as file://.
    let mut repo_url = git_out(&world.project, &["remote", "get-url", "origin"]);
    if repo_url.starts_with('/') || repo_url.starts_with('.') {
        repo_url = format!("file://{repo_url}");
    }
    let sha = git_out(&world.project, &["rev-parse", "HEAD"]);

    // `gantry run -- cargo test`: argv[2..] as the dispatch hands it over.
    let _dispatch_env =
        DispatchEnv::enter(&world.project, &world.home(), world._realbin_dir.path());
    let exit_code = gantry::cli::run::cli(
        &["--".to_string(), "cargo".to_string(), "test".to_string()],
        &repo_url,
        &sha,
    );
    drop(_dispatch_env);

    assert_ne!(
        exit_code, 0,
        "the caller never observes a passing exit from a terminal InfraFailure \
         — the only 0 this tail may return is the local rerun's own earned \
         one, and the fixture suite fails (exit {exit_code})"
    );
    assert_eq!(
        exit_code, 101,
        "the caller's exit is the local rerun's own failing code (exit \
         {exit_code}; the captured stderr carries the ladder's lines)"
    );

    // The terminal verdict is flight-recorded at the remote-verdict stage
    // before the ladder runs — exactly once, since the local rerun below
    // spawns cleanly and writes no bundle of its own.
    let crash_dirs = world.crash_dirs();
    assert_eq!(
        crash_dirs.len(),
        1,
        "the terminal verdict is flight-recorded exactly once, got: {crash_dirs:?}"
    );
    let events = fs::read_to_string(crash_dirs[0].join("events.jsonl"))
        .expect("read the bundle's events.jsonl");
    assert!(
        events.contains("\"stage\":\"remote-verdict\""),
        "the bundle names the remote-verdict stage, got:\n{events}"
    );

    let verdicts = world.verdict_records();
    assert_eq!(
        verdicts.len(),
        1,
        "exactly one terminal record, got: {verdicts:?}"
    );
    assert_eq!(
        verdicts[0]["verdict"], "test_failure",
        "the ledger records the local rerun's own failing verdict — the \
         trailer's TestFailure, observable in the ledger"
    );
    assert_eq!(
        verdicts[0]["ran"], "local_after_infra",
        "the terminal record is the ladder's, not the remote attempt's — the \
         degrade through resolve_and_fall_back, observable in the ledger"
    );
    assert_eq!(
        verdicts[0]["exit_code"], exit_code,
        "the recorded exit code is the same failing code the caller got"
    );
    assert!(
        verdicts[0].get("timeout").is_none(),
        "a terminal verdict raises no expiry — no timeout detail, got: {verdicts:?}"
    );
}
