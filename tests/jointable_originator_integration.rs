// gantry — integration tests for the JoinTable originator (miss) path
// (plan Component 9, bead gantry-03f6d99d, split child 1/3 of gantry-109bb529).
//
// Drives the real binary through the intercepted `cargo test` path (argv[0]
// = cargo, the same harness shape as integration.rs) against mock CI
// commands, and watches the join-table state dir the run actually uses (an
// isolated HOME redirects it), proving the originator entry lifecycle end to
// end:
//
// - **In flight, the key is visible**: the claim's entry exists under
//   `<state>/join/` while the run runs — one live attachment — and carries
//   the submitted handle once the originator records it, so waiters have
//   something to stream and wait.
// - **Terminal, the key is gone**: the entry guard lives to the end of the
//   run function, so the entry clears after the verdict.
//
// The parent's other acceptance clauses are their own split children, and
// their properties are deliberately not asserted here: the dispatch-failure
// non-wedge proof is gantry-5d643d82 and the originator-death reclaim proof
// is gantry-e6edc6cb.
//
// The join key is deliberately not recomputed here: each run gets a fresh
// isolated HOME whose join table holds exactly one claim, so globbing
// `<state>/join/*.run` observes it without duplicating the hash derivation
// the production path already owns. (The entry file is the visibility
// witness this child asserts; `attachment_count` reads the same state under
// the claim lock.)
//
// Each test gets its own temp dirs and bakes paths into its mock scripts —
// no shared state, no env vars — so the suite stays hermetic under parallel
// `cargo test` (the same posture as command_backend_integration.rs, bf-3rer).

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The handle the mock submit reports — what `record_handle` must land in
/// the entry, and what the mock wait must be parked on.
const HANDLE: &str = "wf-origin-1";

/// How long a test waits for the in-flight run to reach a checkpoint (entry
/// visible, handle recorded). Generous: the child does real git work (gate,
/// ref push) before the entry even exists, and CI boxes are slow; the 25 ms
/// poll interval keeps the happy path fast.
const APPEAR_BUDGET: Duration = Duration::from_secs(30);

/// A fixture: everything one intercepted run needs, isolated per test.
///
/// The mock command templates are baked into the user-layer config up front
/// (`[remote.command]` is user-layer only — trust boundary S-2), so a test
/// flips dispatch behavior by rewriting `<work>/mock-submit`, never by
/// reparsing config.
struct Fixture {
    /// Isolated HOME: user config plus the join-table state dir.
    home: TempDir,
    /// The bare remote the epoch refs push to (held for cleanup only).
    _remote: TempDir,
    /// The fixture repo the intercepted run executes in.
    repo: TempDir,
    /// Mock scripts and the release flag the mock wait parks on.
    work: TempDir,
    /// `repo/cargo` → the gantry binary (argv[0] = cargo selects the
    /// intercepted profile).
    cargo_link: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().expect("temp HOME");
        let remote = TempDir::new().expect("temp remote");
        let repo = TempDir::new().expect("temp repo");
        let work = TempDir::new().expect("temp work dir");

        let submit = work.path().join("mock-submit");
        let logs = work.path().join("mock-logs");
        let wait = work.path().join("mock-wait");
        let gantry_config = home.path().join(".config/gantry");
        fs::create_dir_all(&gantry_config).expect("create user config dir");
        fs::write(
            gantry_config.join("config.toml"),
            format!(
                "[remote]\nbackend = \"command\"\n\n[remote.command]\nsubmit = [\"{}\", \"{{repo}}\", \"{{rev}}\", \"{{args_json}}\"]\nlogs = [\"{}\", \"{{handle}}\"]\nwait = [\"{}\", \"{{handle}}\"]\n",
                script_path(&submit),
                script_path(&logs),
                script_path(&wait),
            ),
        )
        .expect("write user config opting into the mock command backend");

        write_script(&submit, &good_submit_body(work.path()));
        write_script(&logs, "#!/usr/bin/env bash\necho \"log line for $1\"\n");
        write_script(&wait, &blocking_wait_body(work.path()));

        git(
            repo.path(),
            &[
                "init",
                "--bare",
                remote.path().to_str().expect("remote path utf-8"),
            ],
        );
        git(repo.path(), &["init"]);
        git(
            repo.path(),
            &["config", "user.name", "JoinTable Originator Tests"],
        );
        git(
            repo.path(),
            &["config", "user.email", "jointable-originator@test"],
        );
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                remote.path().to_str().expect("remote path utf-8"),
            ],
        );
        fs::write(repo.path().join("lib.rs"), "pub fn f() {}\n").expect("write fixture source");

        // The cargo symlink is part of the fixture tree (created before the
        // commit below), so the working tree stays clean for the GitGate.
        let cargo_link = repo.path().join("cargo");
        std::os::unix::fs::symlink(gantry_binary(), &cargo_link).expect("symlink gantry as cargo");

        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);

        Fixture {
            home,
            _remote: remote,
            repo,
            work,
            cargo_link,
        }
    }
}

fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

fn script_path(p: &Path) -> String {
    p.to_str().expect("temp path is valid utf-8").to_string()
}

fn write_script(path: &Path, body: &str) {
    let mut file = fs::File::create(path).expect("create mock script");
    file.write_all(body.as_bytes()).expect("write mock script");
    let mut perm = fs::metadata(path).expect("stat mock script").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("make mock script executable");
}

/// Submit mock: records its argv and reports the well-known handle.
fn good_submit_body(work: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"{}/submit-argv.txt\"\necho '{}'\n",
        script_path(work),
        HANDLE
    )
}

/// Submit mock for the failed-dispatch path: it never returns a handle, so
/// the originator must leave the join key behind only until its guard drops.
fn failing_submit_body() -> &'static str {
    "#!/usr/bin/env bash\necho 'mock submit failed' >&2\nexit 42\n"
}

/// Submit mock for the killed-originator path: signal that the originator
/// reached dispatch, then hold the claim handle-less until the test kills the
/// gantry process. The release file lets the orphaned mock child exit after
/// its parent is gone, so the test leaves no fixture process behind.
fn blocked_submit_body(work: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\ntouch \"{}/submit-started\"\ni=0\nwhile [ ! -f \"{}/submit-release\" ]; do\n  i=$((i + 1))\n  [ \"$i\" -ge 600 ] && exit 42\n  sleep 0.05\ndone\ntouch \"{}/submit-finished\"\nexit 42\n",
        script_path(work),
        script_path(work),
        script_path(work),
    )
}

/// Wait mock: records its argv, then parks until `<work>/release` appears
/// (so a test can observe the in-flight key before the verdict) and exits 0
/// (Pass). Self-terminating: if the release never arrives the mock fails
/// with exit 2 (InfraFailure) after 30 s instead of hanging the suite.
fn blocking_wait_body(work: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"{}/wait-argv.txt\"\ni=0\nwhile [ \"$i\" -lt 600 ]; do\n  [ -f \"{}/release\" ] && exit 0\n  i=$((i + 1))\n  sleep 0.05\ndone\necho 'mock wait: release never arrived' >&2\nexit 2\n",
        script_path(work),
        script_path(work)
    )
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Spawn the intercepted run (`cargo test` through the gantry symlink) in
/// the fixture repo, under the isolated HOME. Ambient GANTRY_* switches are
/// stripped so the run takes the default arms (dedup on, remote eligible)
/// no matter what the invoking environment carries.
fn spawn_run(f: &Fixture) -> std::process::Child {
    Command::new(&f.cargo_link)
        .current_dir(f.repo.path())
        .arg("test")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", f.home.path())
        // XDG overrides would redirect the config/state layers away from the
        // isolated HOME; the GANTRY_* switches would override the decision
        // arms under test. Strip rather than set.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("GANTRY_LOCAL")
        .env_remove("GANTRY_ON")
        .env_remove("GANTRY_JOIN")
        .env_remove("GANTRY_MEMOIZE")
        .env_remove("GANTRY_FRESH")
        .spawn()
        .expect("spawn gantry run")
}

/// The live entry documents (`.run`) under the fixture's join dir — the
/// in-flight attachments. An absent join dir reads as empty.
fn entries(home: &Path) -> Vec<PathBuf> {
    let join = home.join(".local/state/gantry/join");
    let mut found: Vec<PathBuf> = fs::read_dir(&join)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "run"))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

fn read_entry(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read entry document"))
        .expect("entry document is JSON")
}

/// Assert a run output is the Pass round trip: exit 0 and the verdict
/// trailer on stderr (the trailer is the pipeline's own claim, so the test
/// fails with evidence rather than a bare exit-code mismatch).
fn assert_pass(output: &std::process::Output, context: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{context}: expected the Pass exit code, got {:?}; stderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("[gantry] verdict: Pass"),
        "{context}: expected the Pass verdict trailer; stderr:\n{stderr}"
    );
}

/// Assert the submit-failure exit without accepting a generic non-zero exit
/// as evidence that the pipeline classified the failure correctly.
fn assert_infra_failure(output: &std::process::Output, context: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{context}: expected the InfraFailure exit code, got {:?}; stderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("[gantry] verdict: InfraFailure"),
        "{context}: expected the InfraFailure verdict trailer; stderr:\n{stderr}"
    );
}

#[test]
fn originator_entry_is_visible_in_flight_and_gone_after_terminal() {
    let f = Fixture::new();

    // The run claims before it dispatches and holds the entry until its
    // guard drops at the end of the run function — and the mock wait parks
    // the run mid-flight until the test releases it, so everything between
    // claim and verdict is observable here.
    let mut child = spawn_run(&f);

    // In flight: exactly one live attachment (the originator's), owned by
    // the child process, carrying the submitted handle once the originator
    // records it.
    let deadline = Instant::now() + APPEAR_BUDGET;
    let recorded = loop {
        if let [only] = entries(f.home.path()).as_slice() {
            let doc = read_entry(only);
            if doc["handle"] == HANDLE {
                break doc;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("reap killed run");
            panic!(
                "the originator's entry never appeared with its recorded handle; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(
        recorded["schema"], 1,
        "the entry is this binary's schema, not a foreign document"
    );
    assert_eq!(
        recorded["backend"], "command",
        "the entry names the backend the handle belongs to"
    );
    assert_eq!(
        recorded["pid"].as_u64(),
        Some(u64::from(child.id())),
        "the entry names the originator process"
    );
    assert!(
        !recorded["run_id"].as_str().unwrap_or_default().is_empty(),
        "the entry carries the originator's runlog run id"
    );

    // Still live immediately before the verdict: the attach window spans
    // the whole run, not just the dispatch.
    assert_eq!(
        entries(f.home.path()).len(),
        1,
        "the attachment stays visible while the run is in flight"
    );

    // Terminal: release the parked wait and let the run reach its verdict.
    fs::write(f.work.path().join("release"), b"").expect("write release flag");
    let output = child.wait_with_output().expect("wait for gantry run");
    assert_pass(&output, "the originator run");

    // The guard dropped with the run function: the key is closed.
    assert!(
        entries(f.home.path()).is_empty(),
        "the entry clears once the run reaches a terminal verdict"
    );

    // The verdict was waited out on the recorded handle — the same handle a
    // joining waiter would stream and wait.
    let wait_argv =
        fs::read_to_string(f.work.path().join("wait-argv.txt")).expect("the mock wait ran");
    assert!(
        wait_argv.contains(HANDLE),
        "the run waited on the recorded handle, got: {wait_argv}"
    );
}

#[test]
fn failed_submit_clears_key_for_the_next_originator() {
    let f = Fixture::new();

    // The first invocation claims the key and reaches submit, but dispatch
    // fails before a handle can be recorded.
    write_script(&f.work.path().join("mock-submit"), failing_submit_body());
    let first = spawn_run(&f);
    let first_output = first
        .wait_with_output()
        .expect("wait for failed originator run");
    assert_infra_failure(&first_output, "the failed originator run");

    // The early submit error returned through the originator guard's scope:
    // there is no handle-bearing (or handle-less) claim left to attach to.
    assert!(
        entries(f.home.path()).is_empty(),
        "a failed submit must remove the originator entry immediately"
    );

    // Re-enable dispatch and invoke the exact same command again. It must
    // claim Originator afresh, rather than attaching to a dead handle or
    // remaining blocked by the failed claim.
    write_script(
        &f.work.path().join("mock-submit"),
        &good_submit_body(f.work.path()),
    );
    let mut second = spawn_run(&f);
    let deadline = Instant::now() + APPEAR_BUDGET;
    let second_entry = loop {
        if let [only] = entries(f.home.path()).as_slice() {
            let doc = read_entry(only);
            if doc["handle"] == HANDLE {
                break doc;
            }
        }
        if Instant::now() >= deadline {
            let _ = second.kill();
            let output = second.wait_with_output().expect("reap stuck second run");
            panic!(
                "the next identical invocation did not become an originator; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(
        second_entry["handle"], HANDLE,
        "the next invocation recorded its own submitted handle"
    );
    fs::write(f.work.path().join("release"), b"").expect("write release flag");
    let second_output = second
        .wait_with_output()
        .expect("wait for recovered originator run");
    assert_pass(&second_output, "the recovered originator run");
    let second_stderr = String::from_utf8_lossy(&second_output.stderr);
    assert!(
        !second_stderr.contains("joining in-flight run"),
        "the recovered invocation must not attach to a dead run; stderr:\n{}",
        second_stderr
    );
    assert!(
        entries(f.home.path()).is_empty(),
        "the recovered originator must clear its entry at terminal"
    );
}

#[test]
fn killed_originator_claim_is_reclaimed_by_the_next_originator() {
    let f = Fixture::new();

    // Hold submit before it returns a handle. The entry is therefore a real
    // claimed key, but still eligible for dead-owner takeover; killing the
    // gantry process bypasses JoinEntry::drop just as SIGKILL does in use.
    write_script(
        &f.work.path().join("mock-submit"),
        &blocked_submit_body(f.work.path()),
    );
    let mut first = spawn_run(&f);
    let deadline = Instant::now() + APPEAR_BUDGET;
    let first_entry = loop {
        let started = f.work.path().join("submit-started");
        if started.exists() {
            if let [only] = entries(f.home.path()).as_slice() {
                let doc = read_entry(only);
                if doc["handle"] == "" {
                    break doc;
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            let _ = first.wait();
            let _ = fs::write(f.work.path().join("submit-release"), b"");
            panic!(
                "the originator never reached a handle-less claimed submit; entries: {:?}",
                entries(f.home.path())
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let first_pid = first.id();

    // Child::kill is SIGKILL on Unix: no Rust destructor runs, so the entry
    // remains as the stale claim the next invocation must reclaim.
    first.kill().expect("SIGKILL the claimed originator");
    let first_output = first
        .wait_with_output()
        .expect("reap the killed originator");
    assert!(
        !first_output.status.success(),
        "the originator must not exit cleanly after SIGKILL"
    );
    assert_eq!(
        entries(f.home.path()).len(),
        1,
        "SIGKILL bypasses the guard, leaving the claimed entry for reclaim"
    );

    // Let the submit helper, which was orphaned by SIGKILL, finish before
    // replacing the script for the takeover's successful dispatch.
    fs::write(f.work.path().join("submit-release"), b"")
        .expect("release the orphaned submit helper");
    let helper_deadline = Instant::now() + Duration::from_secs(5);
    while !f.work.path().join("submit-finished").exists() {
        assert!(
            Instant::now() < helper_deadline,
            "the orphaned submit helper did not exit after release"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // The identical invocation must take over the dead, handle-less claim as
    // a fresh Originator and reach submit. A joiner would emit the attach
    // diagnostic and would never create this new handle-bearing entry.
    write_script(
        &f.work.path().join("mock-submit"),
        &good_submit_body(f.work.path()),
    );
    let mut second = spawn_run(&f);
    let second_deadline = Instant::now() + APPEAR_BUDGET;
    let second_entry = loop {
        if let [only] = entries(f.home.path()).as_slice() {
            let doc = read_entry(only);
            if doc["handle"] == HANDLE {
                break doc;
            }
        }
        if Instant::now() >= second_deadline {
            let _ = second.kill();
            let output = second.wait_with_output().expect("reap stuck takeover run");
            panic!(
                "the next invocation did not reclaim the killed originator; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(
        first_entry["pid"].as_u64(),
        Some(u64::from(first_pid)),
        "the stale entry belonged to the killed first process"
    );
    assert_eq!(
        second_entry["pid"].as_u64(),
        Some(u64::from(second.id())),
        "the second invocation rewrote the claim as the new originator"
    );
    assert_ne!(
        second_entry["pid"], first_entry["pid"],
        "the reclaimed entry must not retain the dead originator"
    );
    assert!(
        f.work.path().join("submit-argv.txt").exists(),
        "the fresh originator reached the backend submit"
    );
    fs::write(f.work.path().join("release"), b"").expect("release takeover wait");
    let second_output = second
        .wait_with_output()
        .expect("wait for reclaimed originator run");
    assert_pass(&second_output, "the reclaimed originator run");
    let second_stderr = String::from_utf8_lossy(&second_output.stderr);
    assert!(
        !second_stderr.contains("joining in-flight run"),
        "the takeover must submit as an originator, not attach to the dead run; stderr:\n{}",
        second_stderr
    );
    assert!(
        entries(f.home.path()).is_empty(),
        "the reclaimed originator must clear the stale key at terminal"
    );
}
