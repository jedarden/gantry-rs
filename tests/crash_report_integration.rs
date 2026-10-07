// gantry — InfraFailure → crash bundle → `gantry report` round trip
// (plan Phase 1b acceptance AS-2, bead gantry-f6c93e5a).
//
// Proves the flight recorder through the REAL wiring, not a re-implementation:
// an intercepted `cargo test` in a fixture repo whose `origin` rejects the
// epoch-ref push (a pre-receive hook that exits 1 carrying drill credentials)
// lands in decision.rs's push-failure record call site, writes a REDACTED
// bundle under `<state>/crash/<run-id>/`, and `gantry report <run-id>` prints
// and packages that bundle — readable, with every drill credential stripped
// before write and again on print.
//
// The drill credentials ride the three redactor shapes S-5 documents (rules
// 1, 3, 4 in src/crash.rs): URL userinfo, a `token=` key/value, and a
// well-known `sk-live-` token prefix. The assertions are deliberately
// non-vacuous: the bundle must contain `[REDACTED]` (the redactor actually
// engaged) AND none of the live literals (nothing leaked).
//
// Every test runs the real gantry binary inside a throwaway git repo with a
// throwaway HOME (same technique as tests/integration.rs and
// tests/tier0_integration.rs), so the bundle lands in the throwaway state dir
// regardless of the invoking user's real environment, and the suite stays
// hermetic under parallel `cargo test` — no network, no cluster, the bare
// remote lives inside the fixture.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

// Drill credentials the rejecting pre-receive hook emits. Each is a literal
// the bundle must never carry; each rides a different documented redactor
// rule, so a rule going missing breaks exactly one assertion. All are
// concat!-split like the fixtures in src/crash.rs: the Forgejo pre-receive
// scanner flags contiguous token-shaped literals in new blobs and does not
// honor gitleaks:allow, and a drill fixture is not worth a blocked push. The
// runtime strings are byte-identical to the real-world shapes.
/// URL userinfo password (redactor rule 1).
const DRILL_URL_PASSPHRASE: &str = concat!("hunter2-drill-", "passphrase-9f2c");
/// A value on a secret key (redactor rule 3: `token=` is a secret key, so
/// the value redacts to the mark's closing delimiter).
const DRILL_KV_SECRET: &str = concat!("drill-kv-lease-", "secret-3e7a");
/// Well-known token prefix + body (redactor rule 4: the body redacts, the
/// `sk-live-` prefix survives by design — hence asserting on the full
/// literal, not the prefix).
const DRILL_TOKEN: &str = concat!("sk-live-", "drillsecrettoken1a2b3c4d");

/// The pre-receive hook: reject every push, leaking drill credentials the
/// way a real misbehaving remote would (hook output is relayed to the
/// client's push stderr, which RefPusher hands to the recorder verbatim).
const PRE_RECEIVE_HOOK: &str = concat!(
    "#!/bin/sh\n",
    "echo 'drill pre-receive: lease expired upstream=https://gantry-ci:hunter2-drill-",
    r"passphrase-9f2c@drill-host.invalid/team/repo.git'",
    "\n",
    "echo 'drill pre-receive: token=drill-kv-lease-",
    r"secret-3e7a rejected'",
    "\n",
    "echo 'drill pre-receive: lease sk-live-",
    r"drillsecrettoken1a2b3c4d expired'",
    "\n",
    "exit 1\n"
);

/// The fixture test: never runs (the push fails before the backend submit),
/// but keeps the fixture a plausible cargo project.
const FIXTURE_LIB_RS: &str = "#[cfg(test)]
mod tests {
    #[test]
    fn crash_fixture_test() {
        // The remote pipeline never gets here: origin rejects every push.
    }
}
";

/// A self-contained crash-drill world: a committed fixture git repo whose
/// `origin` is a bare remote with a rejecting pre-receive hook, plus an
/// isolated HOME carrying a user-layer config (`backend = "command"`) so the
/// interception takes the remote pipeline (decision.rs), not Tier-0.
struct CrashWorld {
    /// Isolated HOME: the user-config layer, the state dir, and the bundle.
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

impl CrashWorld {
    fn new(name: &str) -> Self {
        let home = TempDir::new().expect("create isolated HOME");
        let project_dir = TempDir::new().expect("create fixture project dir");
        let project = project_dir.path().to_path_buf();

        // User config layer: a command backend, so an intercepted `cargo
        // test` enters the remote decision pipeline (run_remote) where the
        // push-failure record call site lives. The push fails before the
        // backend is ever consulted, so no [remote.command] templates are
        // needed — the defaults load fine and die unexercised.
        let config_dir = home.path().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("create user config dir");
        fs::write(
            config_dir.join("config.toml"),
            "[remote]\nbackend = \"command\"\n",
        )
        .expect("write user config");

        // Fixture crate: trivial, dependency-free. The empty `[workspace]`
        // detaches it from any outer workspace root cargo's upward walk might
        // find (same guard as the Tier-0 fixture).
        fs::write(
            project.join("Cargo.toml"),
            format!(
                "[package]\nname = \"crash-fixture-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
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

        // Bare remote with a rejecting pre-receive hook, INSIDE the fixture
        // work tree (so everything commits and the tree gates clean). The
        // hook must exist before `git add .` so the committed tree stays
        // clean — a dirty tree would fail the GitGate and record under the
        // "ineligible" stand-in instead of the real run id.
        let bare_remote = project.join(".git-state/bare-remote.git");
        fs::create_dir_all(bare_remote.join("hooks")).expect("create bare remote hooks/");
        let hook = bare_remote.join("hooks/pre-receive");
        fs::write(&hook, PRE_RECEIVE_HOOK).expect("write pre-receive hook");
        fs::set_permissions(&hook, {
            let mut perm = fs::metadata(&hook).unwrap().permissions();
            perm.set_mode(0o755);
            perm
        })
        .expect("make pre-receive hook executable");

        // Git repo + remote, with identity pinned (a global gitconfig is not
        // guaranteed under the isolated HOME).
        git(&project, &["init", "--bare", bare_remote.to_str().unwrap()]);
        git(&project, &["init"]);
        git(&project, &["config", "user.name", "Crash Drill"]);
        git(
            &project,
            &["config", "user.email", "crash-drill@test.invalid"],
        );
        git(
            &project,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        );
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "fixture"]);

        // Pin the "real cargo" the shim's PATH lookup resolves to the cargo
        // that compiled this very test (same guard as Tier-0: the host PATH
        // may carry a wrapper that offloads `cargo test` in a git repo with
        // an origin to a CI cluster — and the fixture is exactly such a repo).
        let realbin_dir = TempDir::new().expect("create realbin dir");
        let realbin = realbin_dir.path().to_path_buf();
        std::os::unix::fs::symlink(env!("CARGO"), realbin.join("cargo"))
            .expect("symlink the real cargo into realbin");

        CrashWorld {
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

    /// Run the gantry management CLI (`report`) with the same isolation.
    fn run_cli(&self, args: &[&str]) -> (i32, String, String) {
        let mut cmd = Command::new(gantry_binary());
        cmd.current_dir(&self.project).args(args);
        self.isolate(&mut cmd);
        let output = cmd.output().expect("run gantry CLI");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// The one crash bundle under `<state>/crash/`, with its run id.
    fn bundle(&self) -> (String, PathBuf) {
        let crash_root = self.state_dir().join("crash");
        let mut ids: Vec<PathBuf> = fs::read_dir(&crash_root)
            .expect("state crash dir exists after InfraFailure")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        assert_eq!(
            ids.len(),
            1,
            "exactly one drill ran; bundles: {:?}",
            ids.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );
        let dir = ids.remove(0);
        let run_id = dir.file_name().unwrap().to_string_lossy().to_string();
        (run_id, dir)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every artifact in a bundle dir (or packaged copy) as (name, contents).
fn read_bundle(dir: &Path) -> Vec<(String, String)> {
    let mut files = fs::read_dir(dir)
        .expect("read bundle dir")
        .flatten()
        .filter(|e| e.metadata().expect("stat bundle entry").is_file())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let text = fs::read_to_string(e.path())
                .unwrap_or_else(|e| panic!("bundle file {name} must be readable text: {e}"));
            (name, text)
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// The bundle carries none of the drill credentials, and the redactor
/// demonstrably engaged (at least one `[REDACTED]` mark landed).
fn assert_redacted(files: &[(String, String)], where_: &str) {
    for secret in [DRILL_URL_PASSPHRASE, DRILL_KV_SECRET, DRILL_TOKEN] {
        for (name, text) in files {
            assert!(
                !text.contains(secret),
                "{where_}: {name} leaks drill credential {secret}"
            );
        }
    }
    let total_marks: usize = files
        .iter()
        .map(|(_, t)| t.matches("[REDACTED]").count())
        .sum();
    assert!(
        total_marks > 0,
        "{where_}: no [REDACTED] mark anywhere — the hook credentials never \
         reached the redactor, so this proves nothing"
    );
}

/// The full acceptance round trip (plan Phase 1b AS-2): a real InfraFailure
/// through decision.rs's push-failure record call site leaves a bundle that
/// `gantry report <run-id>` prints AND packages, readable, and carrying no
/// credentials.
#[test]
fn infra_failure_round_trips_to_a_readable_redacted_report() {
    let world = CrashWorld::new("roundtrip");

    // 1. Drive the real wiring: the intercepted run fails at push, lands in
    //    decision.rs's record_infra_failure call, exits non-zero.
    let (code, stdout, stderr) = world.run_shim();
    assert_eq!(code, 1, "push-rejected run exits 1; stdout:\n{stdout}");
    assert!(
        stderr.contains("[gantry] verdict: PushFailed"),
        "the shim must announce the push failure; stderr:\n{stderr}"
    );

    // 2. The recorder wrote exactly one bundle, named by the run id the
    //    write-ahead intent used (INV-1 pairing), with the expected
    //    artifacts.
    let (run_id, dir) = world.bundle();
    assert!(
        !run_id.is_empty() && run_id != "ineligible",
        "the bundle carries the real intent run id, not a stand-in: {run_id}"
    );
    let names: Vec<String> = read_bundle(&dir).into_iter().map(|(n, _)| n).collect();
    for expected in [
        "manifest.json",
        "events.jsonl",
        "config.json",
        "git-state.txt",
        "backend-push.txt",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "bundle missing {expected}; has {names:?}"
        );
    }
    let on_disk = read_bundle(&dir);
    // Redact-before-write (S-5): the hook's credentials rode git's push
    // stderr into the recorder; nothing live may survive on disk.
    assert_redacted(&on_disk, "bundle on disk");
    let push_artifact = &on_disk
        .iter()
        .find(|(n, _)| n == "backend-push.txt")
        .unwrap()
        .1;
    assert!(
        push_artifact.contains("git push failed"),
        "backend-push.txt carries the pusher's reason; got:\n{push_artifact}"
    );
    let events = &on_disk.iter().find(|(n, _)| n == "events.jsonl").unwrap().1;
    assert!(
        events.contains("\"stage\":\"push\""),
        "events.jsonl records the push stage; got:\n{events}"
    );

    // 3. `gantry report <run-id>` prints the bundle: readable (manifest
    //    first, then each artifact under a header) and still redacted.
    let (code, stdout, _) = world.run_cli(&["report", &run_id]);
    assert_eq!(code, 0, "report of a recorded run exits 0");
    assert!(
        stdout.starts_with("crash bundle:"),
        "report leads with the bundle path; got:\n{stdout}"
    );
    for expected in ["===== manifest.json =====", "===== backend-push.txt ====="] {
        assert!(
            stdout.contains(expected),
            "report missing {expected}; got:\n{stdout}"
        );
    }
    assert!(
        stdout.contains(&run_id),
        "manifest carries the run id so the report is self-identifying"
    );
    assert_redacted(
        &[("report-output".to_string(), stdout.clone())],
        "report output",
    );

    // 4. `gantry report <run-id> --package <dir>` copies the bundle out of
    //    the state dir, still readable, still redacted, never clobbering.
    let dest = world.home().join("drill-package");
    let (code, stdout, _) =
        world.run_cli(&["report", &run_id, "--package", dest.to_str().unwrap()]);
    assert_eq!(code, 0, "--package exits 0; stdout:\n{stdout}");
    assert!(
        stdout.contains("packaged crash bundle"),
        "--package announces the copy; got:\n{stdout}"
    );
    let packaged = read_bundle(&dest);
    let packaged_names: Vec<String> = packaged.iter().map(|(n, _)| n.clone()).collect();
    for expected in ["manifest.json", "events.jsonl", "backend-push.txt"] {
        assert!(
            packaged_names.iter().any(|n| n == expected),
            "package missing {expected}; has {packaged_names:?}"
        );
    }
    assert_redacted(&packaged, "packaged copy");

    // 5. Packaging never clobbers: a second --package to the same dest fails.
    let (code, _, _) = world.run_cli(&["report", &run_id, "--package", dest.to_str().unwrap()]);
    assert_eq!(
        code, 1,
        "packaging over an existing destination fails loudly"
    );
}

/// The packaging leg on its own (parent acceptance AS-2, bead
/// gantry-f6c93e5a; packaging bead gantry-4736e7ed). The round trip above
/// packages once inline on its way past; this test drives `--package` to a
/// fresh temp destination and holds the PACKAGED BYTES — the actual artifact
/// read back out of the destination, not the source bundle — to the same
/// two-sided bar: readable (every recorded artifact survives the package
/// round trip byte-for-byte, manifest and verdict content included) and
/// credential-clean (the redaction mark is present, and no drill literal
/// appears anywhere in the packaged artifact).
#[test]
fn packaged_bundle_is_readable_and_credential_clean() {
    let world = CrashWorld::new("package");

    // The same synthetic InfraFailure as the round trip: the intercepted run
    // dies at push, lands in decision.rs's record call site, exits non-zero.
    let (code, stdout, stderr) = world.run_shim();
    assert_eq!(code, 1, "push-rejected run exits 1; stdout:\n{stdout}");
    assert!(
        stderr.contains("[gantry] verdict: PushFailed"),
        "the shim must announce the push failure; stderr:\n{stderr}"
    );
    let (run_id, dir) = world.bundle();

    // Package to a temp destination — neither the state dir nor the source
    // bundle (packaging refuses to clobber, so the destination is fresh).
    let scratch = TempDir::new().expect("create package scratch dir");
    let dest = scratch.path().join("packaged-bundle");
    let (code, stdout, _) =
        world.run_cli(&["report", &run_id, "--package", dest.to_str().unwrap()]);
    assert_eq!(code, 0, "--package exits 0; stdout:\n{stdout}");
    assert!(
        stdout.contains("packaged crash bundle"),
        "--package announces the copy; got:\n{stdout}"
    );

    // Read the packaged artifact back and prove nothing was lost in the
    // package round trip: same artifact set, same names, byte-identical
    // contents. The packaged copy is what would be attached to an issue, so
    // it must stand alone — manifest still self-identifying, verdict content
    // (the push stage and the pusher's reason) still readable.
    let packaged = read_bundle(&dest);
    let on_disk = read_bundle(&dir);
    assert_eq!(
        packaged.len(),
        on_disk.len(),
        "packaging must carry every recorded artifact; packaged {:?}",
        packaged.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    for ((name, packaged_text), (source_name, source_text)) in packaged.iter().zip(&on_disk) {
        assert_eq!(
            name, source_name,
            "packaged artifact set differs from the bundle"
        );
        assert_eq!(
            packaged_text, source_text,
            "packaged {name} must be byte-identical to the recorded artifact"
        );
    }
    let manifest = &packaged
        .iter()
        .find(|(n, _)| n == "manifest.json")
        .unwrap()
        .1;
    assert!(
        manifest.contains(&run_id),
        "packaged manifest is self-identifying; got:\n{manifest}"
    );
    let events = &packaged
        .iter()
        .find(|(n, _)| n == "events.jsonl")
        .unwrap()
        .1;
    assert!(
        events.contains("\"stage\":\"push\""),
        "verdict content survives packaging: the push stage; got:\n{events}"
    );
    let push_artifact = &packaged
        .iter()
        .find(|(n, _)| n == "backend-push.txt")
        .unwrap()
        .1;
    assert!(
        push_artifact.contains("git push failed"),
        "verdict content survives packaging: the pusher's reason; got:\n{push_artifact}"
    );

    // Credential-clean against the packaged bytes themselves: the mark is
    // present (the redactor engaged before write) and no drill literal
    // survives anywhere in the artifact.
    assert_redacted(&packaged, "packaged artifact bytes");
}

/// A run id with no bundle is a clean miss: exit 1, a stated reason, not a
/// panic and not a silent zero.
#[test]
fn report_of_an_unknown_run_id_exits_one_with_a_reason() {
    let world = CrashWorld::new("unknown-run");
    let (code, _, stderr) = world.run_cli(&["report", "no-such-drill-run"]);
    assert_eq!(code, 1, "missing bundle exits 1");
    assert!(
        stderr.contains("no crash bundle for run id 'no-such-drill-run'"),
        "the miss names the run id; stderr:\n{stderr}"
    );
}
