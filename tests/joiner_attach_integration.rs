//! The joiner's skip phase at the intercepted run path (plan §Component 9,
//! bead gantry-07e04a96, split child 2/3 of gantry-4b77700f; the jointable
//! store itself is gantry-e811aa38 and the attach-handle claim is split
//! child 1/3, gantry-db9df1c6).
//!
//! A second identical invocation — same remote, sha, tool, and args — must
//! attach to the in-flight run instead of resubmitting it: no epoch-ref push
//! (the originator's ref already carries the sha) and no backend submit (the
//! workflow is already running). The joiner takes the originator's RunHandle
//! into the shared wait with zero push/queue durations, so a recording
//! backend observes exactly one submission per joined run and the remote
//! gains exactly one epoch ref.
//!
//! This is only the skip. Streaming the live run's output through the shared
//! handle and the terminal-arm claim release are the attach tail's remaining
//! split children — their properties are
//! deliberately not asserted here.
//!
//! The property is only really exercised across *processes*: the claim site
//! is `decision::run_remote`, which reads HOME for both the runlog and the
//! join table and cannot redirect either in-process. So this harness runs
//! the real binary twice — originator, then joiner — over one fixture repo
//! and one isolated HOME, against a recording executor: a command-template
//! backend whose every invocation appends its subcommand to a recording
//! file (the wire the "sees exactly one submit" assertion reads), whose
//! submit hands back a fixed handle, and whose wait blocks on a barrier
//! file. The wire's `wait` lines are the attach signal: a joiner can only
//! reach its own wait after its claim found the originator's handle-bearing
//! entry, so two wait lines prove the attach happened before the barrier
//! rises — no sleep, no race.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// How long the harness waits for the executor to record a submit — the
/// originator's claim + push + submit round trip through the real binary.
const RECORD_WAIT: Duration = Duration::from_secs(30);

/// How long the harness waits for the joiner's own `wait` line — its claim
/// must find the originator's handle-bearing entry and attach to it.
const ATTACH_WAIT: Duration = Duration::from_secs(30);

/// Watchdog on each spawned invocation: a joiner that never attaches still
/// terminates (its own submit records a second line and the assertions name
/// it), but a wedged binary must fail the test, not hang `cargo test`. Far
/// above the executor's own 60 s wait self-cap.
const EXIT_WAIT: Duration = Duration::from_secs(120);

/// The handle the recording executor hands back for every submit. Both wait
/// lines must carry it: the joiner waiting on anything else would mean it
/// forged its own run rather than riding the originator's.
const ORIGINATOR_HANDLE: &str = "handle-originator";

/// Everything the scenario needs: the fixture repo, the isolated HOME both
/// invocations share (one join table between them), the recording executor
/// and its files, and the join dir the assertions read.
struct JoinFixture {
    _root: TempDir,
    /// The git repo both invocations run in (same cwd → same JoinKey).
    fixture: PathBuf,
    /// The bare remote the epoch ref is pushed to; its ref list is the
    /// no-push evidence.
    bare: PathBuf,
    /// Isolated HOME: user config opting into the command backend, plus the
    /// shared runlog and join-table state dir underneath.
    home: PathBuf,
    /// `cargo`-named symlink to the gantry binary — argv[0] is the shim's
    /// intercepted entry.
    cargo_link: PathBuf,
    /// The recording executor (GANTRY_EXEC_PATH).
    executor: PathBuf,
    /// Append-only invocation log the executor writes; the assertions read
    /// it as the backend's wire.
    rec: PathBuf,
    /// The file the executor's wait polls; the test raises it to release the
    /// joined run to its verdict.
    barrier: PathBuf,
    /// The shared join table: `<home>/.local/state/gantry/join`.
    join_dir: PathBuf,
}

/// Write an executable script file (the executor needs +x; nothing else
/// here does).
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perm = fs::metadata(path).expect("stat script").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("chmod script");
}

/// Run `git` in `cwd`, panicking with its stderr on failure — fixture setup
/// has no graceful degradation.
fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
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

/// Build one scenario's fixture: a tiny committed repo with a bare remote,
/// an isolated HOME opting into the command backend (user layer — trust
/// boundary S-2 keeps that choice out of the repo config), and the recording
/// executor with its recording and barrier paths baked into the script text
/// (no env plumbing to leak across the two invocations).
fn fixture() -> JoinFixture {
    let root = TempDir::new().expect("create scenario root");
    let root_path = root.path().to_path_buf();

    // The fixture repo. Its content is irrelevant — the recording backend
    // never runs cargo — but it must be a real, clean work tree with a
    // remote, because the gate checks run for real before the claim site.
    let fixture = root_path.join("repo");
    fs::create_dir_all(fixture.join("src")).expect("create fixture src");
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"joiner-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write fixture Cargo.toml");
    fs::write(fixture.join("src/lib.rs"), "").expect("write fixture lib.rs");
    fs::write(fixture.join(".gitignore"), "/target\n").expect("write fixture .gitignore");

    let bare = root_path.join("remote.git");
    git(&root_path, &["init", "--bare", bare.to_str().unwrap()]);
    // Keep the push duration observable at the millisecond precision used by
    // the runlog. This hook is the recording fixture's push-side clock
    // anchor; the ref count below remains the actual push assertion.
    write_executable(&bare.join("hooks/pre-receive"), "#!/bin/sh\nsleep 0.02\n");
    git(&fixture, &["init"]);
    git(&fixture, &["config", "user.name", "Joiner Fixture"]);
    git(
        &fixture,
        &["config", "user.email", "joiner-fixture@example.com"],
    );
    git(
        &fixture,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&fixture, &["add", "."]);
    git(&fixture, &["commit", "-m", "fixture"]);

    // Isolated HOME: the user config layer opts into the command backend
    // with no template table, so the binary resolves the default templates
    // through GANTRY_EXEC_PATH — pointed at the recording executor below.
    let home = root_path.join("home");
    let user_config = home.join(".config/gantry");
    fs::create_dir_all(&user_config).expect("create user config dir");
    fs::write(
        user_config.join("config.toml"),
        "[remote]\nbackend = \"command\"\n",
    )
    .expect("write user config");

    // The recording executor. Every invocation appends its subcommand and
    // arguments to the recording file (the wire the assertions read);
    // submit hands back a fixed handle; wait polls the barrier for at most
    // 60 s before returning 0 anyway: a joiner that never attaches must
    // surface as a failed assertion, not a hung test.
    let rec = root_path.join("record.log");
    let barrier = root_path.join("barrier");
    let executor = root_path.join("recording-executor.sh");
    write_executable(
        &executor,
        &format!(
            "#!/bin/sh\n\
             REC={}\n\
             BARRIER={}\n\
             printf '%s %s %s\\n' \"$1\" \"$2\" \"$3\" >> \"$REC\"\n\
             case \"$1\" in\n\
             submit) sleep 0.02; echo '{}' ;;\n\
             wait)\n\
               n=0\n\
               while [ ! -f \"$BARRIER\" ] && [ \"$n\" -lt 600 ]; do\n\
                 sleep 0.1\n\
                 n=$((n+1))\n\
               done\n\
               exit 0 ;;\n\
             *) echo \"unexpected subcommand: $1\" >&2; exit 3 ;;\n\
             esac\n",
            rec.display(),
            barrier.display(),
            ORIGINATOR_HANDLE,
        ),
    );

    // The cargo symlink: run with argv[0] = ".../cargo" so the shim takes
    // the intercepted path this bead wires (not `gantry run`). It lives in
    // the scenario root, not the fixture repo — an untracked file inside the
    // repo would fail the working-tree gate before the claim site.
    let cargo_link = root_path.join("cargo");
    std::os::unix::fs::symlink(PathBuf::from(env!("CARGO_BIN_EXE_gantry")), &cargo_link)
        .expect("symlink cargo -> gantry");

    JoinFixture {
        _root: root,
        cargo_link,
        fixture,
        bare,
        join_dir: home.join(".local/state/gantry/join"),
        home,
        executor,
        rec,
        barrier,
    }
}

/// Spawn one intercepted `cargo test` invocation through the fixture: the
/// real binary, the recording backend, the shared HOME. The `test` argument
/// is what makes the run intercepted at all — an absent argv[1] is "cargo
/// with no subcommand", which falls through to passthrough before any
/// pipeline stage (and any claim site) is reached.
fn spawn_run_with_join(f: &JoinFixture, dedup: bool) -> Child {
    let mut command = Command::new(&f.cargo_link);
    command
        .current_dir(&f.fixture)
        .arg("test")
        .env("GANTRY_EXEC_PATH", f.executor.to_str().unwrap())
        .env("HOME", f.home.to_str().unwrap())
        // No XDG override may steer the user config layer away from the
        // isolated HOME. A disabled dedup switch exercises the existing
        // unjoined dispatch path while keeping the same recording backend.
        .env_remove("GANTRY_JOIN")
        .env_remove("GANTRY_ON")
        .env_remove("GANTRY_LOCAL")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    if !dedup {
        command.env("GANTRY_JOIN", "0");
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn intercepted run")
}

fn spawn_run(f: &JoinFixture) -> Child {
    spawn_run_with_join(f, true)
}

fn spawn_unjoined_run(f: &JoinFixture) -> Child {
    spawn_run_with_join(f, false)
}

/// Read the recording file, if it exists yet.
fn recorded(f: &JoinFixture) -> Vec<String> {
    fs::read_to_string(&f.rec)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Block until the recording carries a line starting with `prefix`, then
/// return the whole recording. Times out with the recording in the panic —
/// a silent timeout would hide which stage never ran.
fn wait_for_recorded_line(f: &JoinFixture, prefix: &str, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = recorded(f);
        if lines.iter().any(|l| l.starts_with(prefix)) {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a {prefix:?} line in {:?}; recorded: {lines:?}",
            f.rec
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The in-flight entry, if the key is currently claimed: the single `*.run`
/// file in the shared join dir.
fn in_flight_entry(f: &JoinFixture) -> Option<PathBuf> {
    let entries = match fs::read_dir(&f.join_dir) {
        Ok(entries) => entries,
        Err(_) => return None,
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|ext| ext == "run"))
}

/// How many epoch refs the bare remote carries — the push wire. One ref is
/// the originator's; a joiner that pushed would add its own
/// `refs/gantry/<epoch>-<sha>` and raise this count.
fn remote_epoch_refs(f: &JoinFixture) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "-C",
            f.bare.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
            "refs/gantry/",
        ])
        .output()
        .expect("spawn git for-each-ref");
    assert!(
        out.status.success(),
        "for-each-ref failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Block until `cond` holds, panicking with `what` on timeout.
fn wait_until(cond: impl Fn() -> bool, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Collect a child's exit code and stderr, failing with both if the exit is
/// not `expected` — a joiner or originator that died early must name its
/// stderr in the failure, or the real defect hides behind the assertion.
fn finish(child: Child, expected: i32, role: &str) -> String {
    let mut child = child;
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_string(&mut stderr).expect("read child stderr");
                }
                let code = status.code().unwrap_or(-1);
                assert_eq!(
                    code, expected,
                    "{role} exited {code}, expected {expected}; stderr:\n{stderr}"
                );
                return stderr;
            }
            None => {
                assert!(
                    Instant::now() < deadline,
                    "{role} did not exit within {EXIT_WAIT:?}"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Read the terminal records from the fixture's append-only runlog.
fn verdict_records(f: &JoinFixture) -> Vec<serde_json::Value> {
    fs::read_to_string(f.home.join(".local/state/gantry/runs.jsonl"))
        .expect("the shared runlog exists under the isolated HOME")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|record: &serde_json::Value| record["rec"] == "verdict")
        .collect()
}

/// Originators and unjoined callers both run the remote dispatch stages. The
/// recording fixture makes each stage measurable: the bare-remote ref proves
/// `RefPusher::push`, the executor wire proves backend `submit`, and the
/// deliberate stage delays keep the millisecond durations from collapsing to
/// zero on a fast local machine.
fn assert_remote_dispatch_record(record: &serde_json::Value, role: &str) {
    assert_eq!(record["ran"], "remote", "{role} must run remotely");
    assert_eq!(record["verdict"], "pass", "{role} must receive Pass");
    let durations = &record["durations_ms"];
    assert!(
        durations.is_object(),
        "{role} must record durations: {record}"
    );
    assert!(
        durations["push"]
            .as_u64()
            .expect("push duration is numeric")
            > 0,
        "{role} push duration must retain the observed RefPusher stage: {record}"
    );
    assert!(
        durations["queue"]
            .as_u64()
            .expect("queue duration is numeric")
            > 0,
        "{role} queue duration must retain the observed submit stage: {record}"
    );
    assert!(
        durations["run"].as_u64().expect("run duration is numeric") > 0,
        "{role} run duration must be recorded: {record}"
    );
}

/// A normal claim makes this invocation the originator. Pin the pre-attach
/// behavior that the later Attach early return must leave intact: one pushed
/// epoch ref, one backend submit, and the remote-stage durations in the ledger.
#[test]
fn originator_records_push_submit_and_durations() {
    let f = fixture();
    let originator = spawn_run(&f);

    let submitted = wait_for_recorded_line(&f, "submit ", RECORD_WAIT);
    assert_eq!(
        submitted
            .iter()
            .filter(|line| line.starts_with("submit "))
            .count(),
        1,
        "the originator submits exactly once: {submitted:?}"
    );
    assert_eq!(
        remote_epoch_refs(&f).len(),
        1,
        "the originator pushes exactly one epoch ref"
    );
    wait_until(
        || recorded(&f).iter().any(|line| line.starts_with("wait ")),
        ATTACH_WAIT,
        "originator never reached its backend wait",
    );
    // Ensure the run timer also crosses the runlog's millisecond boundary.
    std::thread::sleep(Duration::from_millis(10));
    fs::write(&f.barrier, "go").expect("raise barrier");
    finish(originator, 0, "originator");

    let verdicts = verdict_records(&f);
    assert_eq!(verdicts.len(), 1, "one originator verdict: {verdicts:?}");
    assert_remote_dispatch_record(&verdicts[0], "originator");
}

/// Disabling dedup leaves the intercepted pipeline unjoined. Pin that path's
/// existing dispatch contract independently of the Attach case: it still
/// pushes and submits, and it still records the corresponding durations.
#[test]
fn unjoined_run_records_push_submit_and_durations() {
    let f = fixture();
    let unjoined = spawn_unjoined_run(&f);

    let submitted = wait_for_recorded_line(&f, "submit ", RECORD_WAIT);
    assert_eq!(
        submitted
            .iter()
            .filter(|line| line.starts_with("submit "))
            .count(),
        1,
        "the unjoined invocation submits exactly once: {submitted:?}"
    );
    assert_eq!(
        remote_epoch_refs(&f).len(),
        1,
        "the unjoined invocation pushes exactly one epoch ref"
    );
    assert!(
        in_flight_entry(&f).is_none(),
        "an unjoined invocation must not create a join-table entry"
    );
    wait_until(
        || recorded(&f).iter().any(|line| line.starts_with("wait ")),
        ATTACH_WAIT,
        "unjoined invocation never reached its backend wait",
    );
    std::thread::sleep(Duration::from_millis(10));
    fs::write(&f.barrier, "go").expect("raise barrier");
    finish(unjoined, 0, "unjoined invocation");

    let verdicts = verdict_records(&f);
    assert_eq!(verdicts.len(), 1, "one unjoined verdict: {verdicts:?}");
    assert_remote_dispatch_record(&verdicts[0], "unjoined invocation");
}

/// Mainline: originator live, joiner attaches, nothing is pushed or
/// submitted twice, the joiner rides the originator's handle, and the key
/// clears once terminal.
#[test]
fn joiner_skips_push_and_submit_riding_the_originators_handle() {
    let f = fixture();

    // The originator runs until its wait blocks on the barrier.
    let originator = spawn_run(&f);
    let first = wait_for_recorded_line(&f, "submit ", RECORD_WAIT);
    assert_eq!(
        first.iter().filter(|l| l.starts_with("submit ")).count(),
        1,
        "exactly one submit before any joiner exists: {first:?}"
    );
    assert_eq!(
        remote_epoch_refs(&f).len(),
        1,
        "the originator's push is the one epoch ref on the remote"
    );

    // The claim must be visible: one in-flight entry on the shared table.
    // Its run id is the originator's — the discriminator that tells the two
    // terminal records apart in the shared ledger once both runs land.
    let entry = in_flight_entry(&f).expect("originator's claim is in flight");
    let entry_doc: serde_json::Value =
        serde_json::from_slice(&fs::read(&entry).expect("read entry document"))
            .expect("entry document is JSON");
    let originator_run_id = entry_doc["run_id"]
        .as_str()
        .expect("the entry carries the originator's run id")
        .to_string();

    // The joiner: same fixture, same HOME, same argv — a stampeding caller.
    let joiner = spawn_run(&f);

    // Attach is observable without any join-table registration API: a
    // joiner reaches its own backend wait only after its claim found the
    // handle-bearing entry, so a second `wait` line on the wire IS the
    // attach. Only then is the barrier safe to raise — raising it earlier
    // could land the originator's verdict before the joiner ever claimed.
    wait_until(
        || {
            recorded(&f)
                .iter()
                .filter(|l| l.starts_with("wait "))
                .count()
                >= 2
        },
        ATTACH_WAIT,
        "joiner never reached its wait on the shared handle",
    );

    // Both waits drain to the same verdict.
    fs::write(&f.barrier, "go").expect("raise barrier");
    let orig_stderr = finish(originator, 0, "originator");
    let joiner_stderr = finish(joiner, 0, "joiner");

    // The recording backend saw exactly the originator's submit — the joiner
    // skipped the backend submit entirely.
    let lines = recorded(&f);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("submit ")).count(),
        1,
        "a joiner must never resubmit; recorded wire: {lines:?}"
    );

    // And it skipped the epoch-ref push: the remote still carries exactly
    // the originator's ref.
    let refs = remote_epoch_refs(&f);
    assert_eq!(
        refs.len(),
        1,
        "the joiner must not push an epoch ref; remote refs: {refs:?}"
    );

    // Both wait lines carry the originator's handle: the joiner waited on
    // the run it attached to, not one it forged.
    let wait_handles: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("wait "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert_eq!(
        wait_handles,
        vec![ORIGINATOR_HANDLE, ORIGINATOR_HANDLE],
        "both participants must wait on the originator's handle; wire: {lines:?}"
    );

    // This child's boundary: the joiner streams nothing yet (split child 2
    // owns the stream); the only wire entries are the submit and the waits.
    assert!(
        !lines.iter().any(|l| l.starts_with("logs ")),
        "the skip-phase joiner must not touch the backend beyond submit-less \
         wait; wire: {lines:?}"
    );
    assert!(
        !orig_stderr.contains("joining in-flight run"),
        "the originator is not a joiner; stderr:\n{orig_stderr}"
    );
    assert!(
        joiner_stderr.contains("joining in-flight run"),
        "joiner must announce the attach; stderr:\n{joiner_stderr}"
    );

    // Terminal cleanup: the run reached its verdict and the originator's
    // entry guard dropped, so the key is open for the next identical run.
    assert!(
        in_flight_entry(&f).is_none(),
        "the in-flight entry must clear once the joined run reaches terminal"
    );

    // The shared ledger pins the attach arm's returns at record grain: both
    // invocations land exactly one terminal record apiece under the one
    // HOME, and the joiner's carries the originator's handle with zero
    // push/queue durations — the wire-level evidence that the attach branch
    // returned `(originator's RunHandle, 0, 0)` and nothing else rode the
    // dispatch stages.
    let ledger = fs::read_to_string(f.home.join(".local/state/gantry/runs.jsonl"))
        .expect("the shared runlog exists under the isolated HOME");
    let verdicts: Vec<serde_json::Value> = ledger
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|r: &serde_json::Value| r["rec"] == "verdict")
        .collect();
    assert_eq!(
        verdicts.len(),
        2,
        "originator and joiner land exactly one terminal record each: {verdicts:?}"
    );
    let originator_record = verdicts
        .iter()
        .find(|r| r["run_id"] == originator_run_id.as_str())
        .unwrap_or_else(|| panic!("no terminal record for run {originator_run_id}"));
    let joiner_record = verdicts
        .iter()
        .find(|r| r["run_id"] != originator_run_id.as_str())
        .expect("the joiner's terminal record is the one not matching the entry's run id");
    for (role, record) in [("originator", originator_record), ("joiner", joiner_record)] {
        assert_eq!(
            record["ran"], "remote",
            "{role} rode the remote wait, not a local fallback"
        );
        assert_eq!(
            record["verdict"], "pass",
            "{role} shares the joined run's Pass verdict"
        );
    }
    assert_eq!(
        joiner_record["handle"], ORIGINATOR_HANDLE,
        "the attach arm's returned handle — the originator's — is what the \
         joiner's terminal record carries"
    );
    assert_eq!(
        originator_record["handle"], ORIGINATOR_HANDLE,
        "the originator's record carries the same handle it submitted"
    );
    assert_remote_dispatch_record(originator_record, "originator");
    let joiner_durations = &joiner_record["durations_ms"];
    assert_eq!(
        joiner_durations["push"], 0,
        "the joiner pushed no epoch ref: zero push duration on its record"
    );
    assert_eq!(
        joiner_durations["queue"], 0,
        "the joiner submitted nothing: zero queue duration on its record"
    );
}
