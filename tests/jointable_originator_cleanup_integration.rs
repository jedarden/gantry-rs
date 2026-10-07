// End-to-end coverage for originator claim cleanup.
//
// These tests drive the real intercepted `cargo` path with an isolated HOME
// and command-backend scripts. That makes the on-disk claim lifecycle
// observable across process boundaries, including a killed originator.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gantry::jointable::{attachment_count, JoinKey};
use tempfile::TempDir;

const HANDLE: &str = "cleanup-handle";
const WAIT_BUDGET: Duration = Duration::from_secs(20);

struct Fixture {
    home: TempDir,
    _remote: TempDir,
    repo: TempDir,
    work: TempDir,
    cargo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().expect("HOME");
        let remote = TempDir::new().expect("remote");
        let repo = TempDir::new().expect("repo");
        let work = TempDir::new().expect("mock work");
        let config_dir = home.path().join(".config/gantry");
        fs::create_dir_all(&config_dir).expect("config directory");

        let submit = work.path().join("submit");
        let logs = work.path().join("logs");
        let wait = work.path().join("wait");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[remote]\nbackend = \"command\"\n\n[remote.command]\nsubmit = [\"{}\"]\nlogs = [\"{}\", \"{{handle}}\"]\nwait = [\"{}\", \"{{handle}}\"]\n",
                submit.display(),
                logs.display(),
                wait.display(),
            ),
        )
        .expect("config");
        write_script(&submit, &good_submit_body(work.path()));
        write_script(&logs, "#!/usr/bin/env bash\nexit 0\n");
        write_script(&wait, &blocking_wait_body(work.path()));

        git(
            repo.path(),
            &[
                "init",
                "--bare",
                remote.path().to_str().expect("remote path"),
            ],
        );
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.name", "cleanup-test"]);
        git(
            repo.path(),
            &["config", "user.email", "cleanup-test@example.invalid"],
        );
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                remote.path().to_str().expect("remote path"),
            ],
        );
        fs::write(repo.path().join("lib.rs"), "pub fn fixture() {}\n").expect("fixture source");

        let cargo = repo.path().join("cargo");
        std::os::unix::fs::symlink(gantry_binary(), &cargo).expect("cargo shim");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "fixture"]);

        Self {
            home,
            _remote: remote,
            repo,
            work,
            cargo,
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.home.path().join(".local/state/gantry")
    }

    fn join_key(&self) -> JoinKey {
        let output = Command::new("git")
            .current_dir(self.repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git revision");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let repo_url = format!("file://{}", self._remote.path().display());
        JoinKey::new(&repo_url, &sha, "cargo", "test", &[])
    }
}

fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

fn write_script(path: &Path, body: &str) {
    let mut file = fs::File::create(path).expect("create script");
    file.write_all(body.as_bytes()).expect("write script");
    let mut permissions = file.metadata().expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script permissions");
}

fn good_submit_body(work: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"{}/submit-argv\"\necho '{}'\n",
        work.display(),
        HANDLE,
    )
}

fn blocking_wait_body(work: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\ni=0\nwhile [ ! -f \"{}/release\" ]; do\n  i=$((i + 1))\n  [ \"$i\" -ge 400 ] && exit 2\n  sleep 0.05\ndone\nexit 0\n",
        work.display(),
    )
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_run(fixture: &Fixture) -> Child {
    Command::new(&fixture.cargo)
        .current_dir(fixture.repo.path())
        .arg("test")
        .env("HOME", fixture.home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("GANTRY_LOCAL")
        .env_remove("GANTRY_ON")
        .env_remove("GANTRY_JOIN")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn intercepted cargo")
}

fn entries(home: &Path) -> Vec<PathBuf> {
    let join = home.join(".local/state/gantry/join");
    let mut paths: Vec<PathBuf> = fs::read_dir(join)
        .map(|read_dir| {
            read_dir
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "run"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn entry_document(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("entry document")).expect("entry JSON")
}

fn wait_for_waiter(fixture: &Fixture) {
    let deadline = Instant::now() + WAIT_BUDGET;
    let att_dir = entries(fixture.home.path())
        .into_iter()
        .next()
        .expect("entry before waiter")
        .with_extension("att");
    loop {
        if fs::read_dir(&att_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "joiner never registered as a waiter"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_entry(fixture: &Fixture, expected_handle: &str) -> serde_json::Value {
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        if let [path] = entries(fixture.home.path()).as_slice() {
            let document = entry_document(path);
            if document["handle"] == expected_handle {
                return document;
            }
        }
        assert!(
            Instant::now() < deadline,
            "originator entry did not record handle; entries: {:?}",
            entries(fixture.home.path())
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn finish(mut child: Child, expected: i32) -> String {
    let deadline = Instant::now() + WAIT_BUDGET;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            let output = child.wait_with_output().expect("collect child");
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert_eq!(status.code(), Some(expected), "stderr: {stderr}");
            return stderr;
        }
        assert!(Instant::now() < deadline, "child did not finish");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn release(fixture: &Fixture) {
    fs::write(fixture.work.path().join("release"), b"").expect("release wait");
}

#[test]
fn originator_entry_clears_after_terminal_verdict() {
    let fixture = Fixture::new();
    let key = fixture.join_key();
    let state_dir = fixture.state_dir();
    let child = spawn_run(&fixture);
    let document = wait_for_entry(&fixture, HANDLE);
    assert_eq!(document["backend"], "command");
    assert_eq!(document["pid"].as_u64(), Some(u64::from(child.id())));
    assert_eq!(entries(fixture.home.path()).len(), 1);
    assert!(
        attachment_count(&state_dir, &key).expect("count in-flight attachments") >= 1,
        "an in-flight originator must be observable as an attachment"
    );

    release(&fixture);
    let stderr = finish(child, 0);
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "stderr: {stderr}"
    );
    assert!(
        entries(fixture.home.path()).is_empty(),
        "terminal originator guard left a claim behind"
    );
    assert_eq!(
        attachment_count(&state_dir, &key).expect("count terminal attachments"),
        0,
        "terminal originator must no longer be observable as an attachment"
    );
}

#[test]
fn failed_dispatch_clears_key_for_the_next_originator() {
    let fixture = Fixture::new();
    let key = fixture.join_key();
    let state_dir = fixture.state_dir();
    write_script(
        &fixture.work.path().join("submit"),
        "#!/usr/bin/env bash\necho submit-failed >&2\nexit 42\n",
    );
    let stderr = finish(spawn_run(&fixture), 1);
    assert!(stderr.contains("verdict: InfraFailure"), "stderr: {stderr}");
    assert!(entries(fixture.home.path()).is_empty());
    assert_eq!(
        attachment_count(&state_dir, &key).expect("count after failed dispatch"),
        0,
        "failed dispatch must release the originator claim"
    );

    write_script(
        &fixture.work.path().join("submit"),
        &good_submit_body(fixture.work.path()),
    );
    let second = spawn_run(&fixture);
    let document = wait_for_entry(&fixture, HANDLE);
    assert_eq!(document["pid"].as_u64(), Some(u64::from(second.id())));
    assert!(
        attachment_count(&state_dir, &key).expect("count second in-flight originator") >= 1,
        "a failed dispatch must not wedge the next originator"
    );
    release(&fixture);
    let stderr = finish(second, 0);
    assert!(
        !stderr.contains("joining in-flight run"),
        "stderr: {stderr}"
    );
    assert!(entries(fixture.home.path()).is_empty());
}

#[test]
fn terminal_joiner_reclaims_entry_after_originator_is_killed() {
    let fixture = Fixture::new();
    let mut originator = spawn_run(&fixture);
    let original = wait_for_entry(&fixture, HANDLE);
    let original_pid = originator.id();

    originator.kill().expect("kill originator");
    let _ = originator.wait().expect("reap originator");
    assert_eq!(entries(fixture.home.path()).len(), 1);

    let joiner = spawn_run(&fixture);
    let joined = wait_for_entry(&fixture, HANDLE);
    wait_for_waiter(&fixture);
    assert_eq!(joined["run_id"], original["run_id"]);
    assert_eq!(joined["pid"].as_u64(), Some(u64::from(original_pid)));

    release(&fixture);
    let stderr = finish(joiner, 0);
    assert!(stderr.contains("joining in-flight run"), "stderr: {stderr}");
    assert!(
        entries(fixture.home.path()).is_empty(),
        "terminal joiner did not reclaim the orphaned originator entry"
    );
}
