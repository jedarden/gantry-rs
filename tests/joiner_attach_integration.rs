//! Regression coverage for the intercepted JoinDecision::Attach path.
//!
//! Two real intercepted invocations share a repository and HOME.  The first
//! invocation submits a fixed handle and blocks in its recording backend.  The
//! second must attach to that handle: the backend wire sees one submit and the
//! bare remote sees one epoch-ref push, while the joiner's terminal record
//! carries the originator handle and zero push/queue durations.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const HANDLE: &str = "recorded-originator";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(45);

struct Fixture {
    _root: TempDir,
    repo: PathBuf,
    remote: PathBuf,
    home: PathBuf,
    cargo: PathBuf,
    executor: PathBuf,
    recording: PathBuf,
    barrier: PathBuf,
}

/// Kill an intercepted child if an assertion aborts the test before the
/// barrier is raised.  Without this guard a failed regression could leave a
/// backend wait process behind for its full timeout.
struct RunningChild {
    child: Child,
}

impl RunningChild {
    fn spawn(fixture: &Fixture) -> Self {
        let child = Command::new(&fixture.cargo)
            .current_dir(&fixture.repo)
            .arg("test")
            .env("HOME", &fixture.home)
            .env("GANTRY_EXEC_PATH", &fixture.executor)
            .env_remove("GANTRY_JOIN")
            .env_remove("GANTRY_LOCAL")
            .env_remove("GANTRY_ON")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn intercepted cargo test");
        Self { child }
    }

    fn finish(mut self) -> String {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            match self.child.try_wait().expect("poll intercepted child") {
                Some(status) => {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = self.child.stderr.take() {
                        pipe.read_to_string(&mut stderr)
                            .expect("read intercepted stderr");
                    }
                    assert_eq!(
                        status.code(),
                        Some(0),
                        "intercepted child failed:\n{stderr}"
                    );
                    return stderr;
                }
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "intercepted child did not exit within {EXIT_TIMEOUT:?}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        if self
            .child
            .try_wait()
            .expect("poll child during cleanup")
            .is_none()
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write recording backend");
    let mut permissions = fs::metadata(path)
        .expect("stat recording backend")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make recording backend executable");
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Fixture {
    let root = TempDir::new().expect("create fixture root");
    let repo = root.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("create fixture repo");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"attach-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write fixture manifest");
    fs::write(repo.join("src/lib.rs"), "").expect("write fixture source");
    fs::write(repo.join(".gitignore"), "/target\n").expect("write fixture gitignore");

    let remote = root.path().join("remote.git");
    git(root.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(&repo, &["init"]);
    git(&repo, &["config", "user.name", "Attach Fixture"]);
    git(
        &repo,
        &["config", "user.email", "attach-fixture@example.com"],
    );
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);

    let home = root.path().join("home");
    let config_dir = home.join(".config/gantry");
    fs::create_dir_all(&config_dir).expect("create fixture config");
    fs::write(
        config_dir.join("config.toml"),
        "[remote]\nbackend = \"command\"\n",
    )
    .expect("write fixture config");

    let recording = root.path().join("recording.log");
    let barrier = root.path().join("barrier");
    let executor = root.path().join("recording-backend.sh");
    write_executable(
        &executor,
        &format!(
            "#!/bin/sh\n\
             printf '%s %s %s\\n' \"$1\" \"$2\" \"$3\" >> '{}'\n\
             case \"$1\" in\n\
             submit) printf '{}\\n' ;;\n\
             wait) while [ ! -f '{}' ]; do sleep 0.01; done ;;\n\
             *) exit 3 ;;\n\
             esac\n",
            recording.display(),
            HANDLE,
            barrier.display(),
        ),
    );

    let cargo = root.path().join("cargo");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_gantry"), &cargo)
        .expect("link cargo shim to gantry");

    Fixture {
        _root: root,
        repo,
        remote,
        home,
        cargo,
        executor,
        recording,
        barrier,
    }
}

fn recorded(fixture: &Fixture) -> Vec<String> {
    fs::read_to_string(&fixture.recording)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn epoch_refs(fixture: &Fixture) -> Vec<String> {
    let output = Command::new("git")
        .args([
            "-C",
            fixture.remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
            "refs/gantry/",
        ])
        .output()
        .expect("inspect pushed epoch refs");
    assert!(output.status.success(), "inspect epoch refs failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn handle_bearing_entry(fixture: &Fixture) -> Option<(PathBuf, serde_json::Value)> {
    let join_dir = fixture.home.join(".local/state/gantry/join");
    let entries = fs::read_dir(join_dir).ok()?;
    entries.filter_map(Result::ok).find_map(|entry| {
        let path = entry.path();
        (path.extension().and_then(|ext| ext.to_str()) == Some("run"))
            .then(|| fs::read(&path).ok())
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|doc: &serde_json::Value| doc["handle"] == HANDLE)
            .map(|doc| (path, doc))
    })
}

fn verdicts(fixture: &Fixture) -> Vec<serde_json::Value> {
    fs::read_to_string(fixture.home.join(".local/state/gantry/runs.jsonl"))
        .expect("shared runlog exists")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|record: &serde_json::Value| record["rec"] == "verdict")
        .collect()
}

#[test]
fn attach_skips_push_and_submit_and_records_originator_handle_with_zero_durations() {
    let fixture = fixture();
    let originator = RunningChild::spawn(&fixture);

    wait_until(
        || {
            recorded(&fixture)
                .iter()
                .any(|line| line.starts_with("submit "))
        },
        "originator submit",
    );
    assert_eq!(
        epoch_refs(&fixture).len(),
        1,
        "originator pushes one epoch ref"
    );

    let handle_deadline = Instant::now() + WAIT_TIMEOUT;
    let (entry_path, entry) = loop {
        if let Some(entry) = handle_bearing_entry(&fixture) {
            break entry;
        }
        assert!(
            Instant::now() < handle_deadline,
            "originator never recorded its backend handle"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let originator_run_id = entry["run_id"]
        .as_str()
        .expect("originator claim has a run id")
        .to_owned();

    let joiner = RunningChild::spawn(&fixture);
    wait_until(
        || {
            recorded(&fixture)
                .iter()
                .filter(|line| line.starts_with("wait "))
                .count()
                >= 2
        },
        "joiner wait on the attached handle",
    );

    fs::write(&fixture.barrier, "release").expect("release recording waits");
    let originator_stderr = originator.finish();
    let joiner_stderr = joiner.finish();
    assert!(!originator_stderr.contains("joining in-flight run"));
    assert!(joiner_stderr.contains("joining in-flight run"));

    let wire = recorded(&fixture);
    assert_eq!(
        wire.iter()
            .filter(|line| line.starts_with("submit "))
            .count(),
        1,
        "Attach must not submit a second backend run: {wire:?}"
    );
    let wait_handles: Vec<&str> = wire
        .iter()
        .filter(|line| line.starts_with("wait "))
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    assert_eq!(wait_handles, vec![HANDLE, HANDLE]);
    assert_eq!(
        epoch_refs(&fixture).len(),
        1,
        "Attach must not push an epoch ref"
    );

    let records = verdicts(&fixture);
    assert_eq!(
        records.len(),
        2,
        "originator and joiner both finish: {records:?}"
    );
    let joiner_record = records
        .iter()
        .find(|record| record["run_id"] != originator_run_id.as_str())
        .expect("find joiner verdict");
    assert_eq!(joiner_record["handle"], HANDLE);
    assert_eq!(joiner_record["durations_ms"]["push"], 0);
    assert_eq!(joiner_record["durations_ms"]["queue"], 0);
    assert!(
        !entry_path.exists(),
        "terminal attach should release the in-flight entry"
    );
}
