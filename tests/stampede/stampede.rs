//! AS-3 stampede harness (plan §6 "fallback admission semaphore", bf-299).
//!
//! A remote outage under fleet load must degrade as a serialized trickle,
//! not a stampede: every fallback run acquires one of a fixed pool of
//! flock-held slot files (default 3 per box), queueing loudly while it
//! waits, and a run that dies — SIGKILL included — releases its slot in the
//! kernel without gantry code running.
//!
//! Flock is a kernel property of open file descriptions, so the property is
//! only really tested across *processes*: this binary is a custom-harness
//! test (`harness = false`) that re-executes itself in three roles.
//!
//! - **parent** (default): builds the fixture repos, spawns the stampede,
//!   and asserts the outcome.
//! - **worker** (argv `--worker`): one fallback invocation via
//!   [`gantry::local::run_fallback`] — the exact production tail of an
//!   `InfraFailure`, queue lines, verdict trailer and exit code included.
//! - **payload** (`STAMPEDE_ID` set, no `--worker`): the "real binary" the
//!   fallback resolves and runs, via the config `real_binary` override — it
//!   stamps `start`/`end` wall-clock events so the parent can recompute
//!   concurrency after the fact.
//!
//! The worker/payload split is argv-based, not env-based, on purpose:
//! `run_fallback` spawns the payload with the worker's environment and an
//! empty argv, so an env flag would leak into the payload and turn it into
//! yet another worker. The parent passes `--worker` to workers; `run_fallback`
//! passes nothing to payloads.
//!
//! The parent measures concurrency from the payload events. A payload's
//! interval lies entirely inside its run's slot hold (the guard is acquired
//! before the payload spawns and dropped after it exits), so payload overlap
//! never overcounts slot holders: `max overlap <= slots` is a sound check of
//! the cap, and `>= 2` proves the pool actually filled (the regression where
//! a queue bug serializes the pool down to one run fails *that* arm).
//!
//! Three scenarios run. The first two use 20 simultaneous fallback
//! invocations:
//!
//! 1. **AS-3 mainline** — all 20 from one repo.
//! 2. **EC-01 two-worktree variant** — 10 + 10 from two linked worktrees of
//!    the same repo (NEEDLE workers share worktrees). The semaphore is a
//!    boxwide pool under the state dir, deliberately *not* keyed by repo or
//!    worktree, so the mixed 20 must still fit through the same 3 slots: a
//!    per-repo pool would have shown overlaps of 6.
//!
//! 3. **SIGKILL leak check** — the "kernel lock release = no leaked slots"
//!    property (plan Component 6): three holders fill the pool, the parent
//!    SIGKILLs them mid-run, and fresh probes must drain through the pool
//!    right away. Nothing gantry-shaped runs at kill time — the kernel alone
//!    drops the flocks — so a leaked slot here is a semaphore bug, and it
//!    fails fast: a probe that cannot get in hits its short bounded wait and
//!    prints the loud `no local slot within` warning the assertions forbid.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long each fallback run's payload holds its slot, in milliseconds.
/// Long enough that the 20 spawns genuinely overlap (spawn cost is ~ms),
/// short enough that a 20-run stampede drains through 3 slots in a few
/// seconds. The SIGKILL scenario's holders override it via
/// `STAMPEDE_HOLD_MS`.
const HOLD_MS: u64 = 400;

/// How long the SIGKILL scenario's slot holders stay alive, in milliseconds —
/// far beyond the parent's patience, since it kills them as soon as their
/// slots are held.
const KILL_HOLD_MS: u64 = 30_000;

/// The bounded wait the SIGKILL scenario's probes run with: short enough that
/// a leaked slot fails the scenario quickly, long enough that a healthy drain
/// (three 400 ms payloads through three freed slots) never trips it.
const KILL_PROBE_WAIT_SECS: u64 = 20;

/// The bounded wait configured on every worker. Far above the stampede's
/// expected drain time; its only job is to bound a wedged queue.
const MAX_WAIT_SECS: u64 = 120;

/// Slot count for every scenario — the plan default, and the number the
/// assertions are written against.
const SLOTS: u32 = 3;

/// Concurrency per scenario: the AS-3 fleet size.
const STAMPEDE_SIZE: usize = 20;

/// Watchdog: a wedged queue must fail the harness, not hang `cargo test`.
const WATCHDOG: Duration = Duration::from_secs(300);

fn main() {
    // Arm the watchdog before anything else; it is forgotten on purpose —
    // it only exists to convert a hang into a failure.
    std::thread::spawn(|| {
        std::thread::sleep(WATCHDOG);
        eprintln!("stampede harness watchdog fired after {WATCHDOG:?}");
        std::process::exit(101);
    });

    // Role dispatch (module docs): the parent's argv decides which role this
    // process plays. `--worker` is a worker; no argv at all plus a
    // STAMPEDE_ID is the payload `run_fallback` spawned; anything else is
    // the parent.
    if std::env::args().nth(1).as_deref() == Some("--worker") {
        worker_main();
    } else if std::env::var("STAMPEDE_ID").is_ok() {
        payload_main();
    } else {
        parent_main();
    }
}

// ============================================================================
// Worker: one fallback invocation
// ============================================================================

/// Run the production fallback tail once — `run_fallback`, with a Tier-0
/// config narrowed to the stampede's slot count and pointed at this same
/// binary as the "real" tool — and exit with its verdict's exit code.
fn worker_main() {
    let id = std::env::var("STAMPEDE_ID").expect("worker needs STAMPEDE_ID");
    let payload_bin = std::path::PathBuf::from(
        std::env::var("STAMPEDE_PAYLOAD").expect("worker needs STAMPEDE_PAYLOAD"),
    );

    let mut config = gantry::config::Config::tier_0_defaults();
    config.local.fallback_slots =
        u32::try_from(env_parse("STAMPEDE_SLOTS", SLOTS as u64)).unwrap_or(SLOTS);
    config.local.fallback_wait_secs = env_parse("STAMPEDE_WAIT_SECS", MAX_WAIT_SECS);
    match config.tools.get_mut("cargo") {
        Some(tool) => tool.real_binary = Some(payload_bin),
        None => {
            eprintln!("stampede worker: tier-0 defaults carry no cargo tool config");
            std::process::exit(100);
        }
    }

    let ctx = gantry::local::FallbackContext {
        runlog: None,
        run_id: &id,
        gate_ms: 0,
        push_ms: 0,
        // A slot-contended fallback, not a deadline expiry: no timeout to name.
        timeout: None,
    };
    let code =
        gantry::local::run_fallback(&config, &[], "stampede drill: backend unreachable", &ctx);
    std::process::exit(code);
}

// ============================================================================
// Payload: the run a slot holder executes
// ============================================================================

/// The "real binary": stamp the slot-held interval, hold the slot for
/// `STAMPEDE_HOLD_MS` (default [`HOLD`]), stamp the end. One file per
/// payload (`$STAMPEDE_EVENTS/<id>.events`), so concurrent payloads cannot
/// interleave mid-line the way appends to a shared log can. The `start` line
/// lands the instant the payload begins — the SIGKILL scenario's parent uses
/// its appearance as proof the worker's slot is held — and the `end` line is
/// appended at exit; the parent only parses files whose payload it saw
/// finish, so the two-write split is never observed half-done.
fn payload_main() {
    let id = std::env::var("STAMPEDE_ID").expect("payload needs STAMPEDE_ID");
    let events_dir = std::env::var("STAMPEDE_EVENTS").expect("payload needs STAMPEDE_EVENTS");
    let hold = env_parse("STAMPEDE_HOLD_MS", HOLD_MS);
    let path = Path::new(&events_dir).join(format!("{id}.events"));
    std::fs::write(&path, format!("start {id} {}\n", unix_ms()))
        .expect("payload can write its start stamp");
    std::thread::sleep(Duration::from_millis(hold));
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("payload can reopen its event file");
    file.write_all(format!("end {id} {}\n", unix_ms()).as_bytes())
        .expect("payload can write its end stamp");
}

/// `u64` from an env var, or the default when unset or unparseable.
fn env_parse(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is past the epoch")
        .as_millis()
}

// ============================================================================
// Parent: the two scenarios
// ============================================================================

/// One spawned fallback invocation and everything the parent needs from it.
struct WorkerRun {
    id: String,
    exit: i32,
    stderr: String,
}

/// One stampede: the invocations to spawn (id + origin cwd) and the isolated
/// state they share.
struct Scenario {
    workers: Vec<(String, PathBuf)>,
    /// Distinct HOME per scenario: each gets a fresh slot pool and ticket
    /// counter, so one scenario's queue can never perturb the other's.
    home: PathBuf,
    /// Directory receiving one `<id>.events` file per payload (module docs
    /// on `payload_main`).
    events: PathBuf,
    /// A copy of this harness binary that plays the payload. A *copy*, not
    /// the original: the worker points `real_binary` at it, and the shim's
    /// re-exec guard (plan §1) would rightly refuse a real binary whose
    /// canonical path equals the fallbacking process's own.
    payload_bin: PathBuf,
}

fn parent_main() {
    let tmp = tempfile::TempDir::new().expect("temp root for the stampede");

    // One repo, two linked worktrees (EC-01): both scenarios run from it,
    // scenario 2 from both worktrees at once.
    let repo = tmp.path().join("repo");
    let second = tmp.path().join("worktree-b");
    build_repo_with_second_worktree(&repo, &second);

    let payload_bin = tmp.path().join("stampede-payload");
    copy_self(&payload_bin);

    println!("scenario 1: {STAMPEDE_SIZE} simultaneous fallbacks, one worktree");
    let single: Vec<(String, PathBuf)> = (0..STAMPEDE_SIZE)
        .map(|i| (format!("single-{i:02}"), repo.clone()))
        .collect();
    let (workers, intervals) = run_scenario(&Scenario {
        workers: single,
        home: tmp.path().join("home-1"),
        events: tmp.path().join("events-1"),
        payload_bin: payload_bin.clone(),
    });
    assert_stampede_outcome(&workers, &intervals);

    println!("scenario 2: {STAMPEDE_SIZE} simultaneous fallbacks across two worktrees");
    let mut mixed: Vec<(String, PathBuf)> = (0..STAMPEDE_SIZE / 2)
        .map(|i| (format!("wt-a-{i:02}"), repo.clone()))
        .collect();
    mixed.extend(
        (0..STAMPEDE_SIZE - STAMPEDE_SIZE / 2).map(|i| (format!("wt-b-{i:02}"), second.clone())),
    );
    let (workers, intervals) = run_scenario(&Scenario {
        workers: mixed,
        home: tmp.path().join("home-2"),
        events: tmp.path().join("events-2"),
        payload_bin: payload_bin.clone(),
    });
    assert_stampede_outcome(&workers, &intervals);

    // EC-01's point: invocations from *both* worktrees of one repo went
    // through the same boxwide pool, and all of them completed.
    let ran_a = workers
        .iter()
        .any(|w| w.id.starts_with("wt-a-") && w.exit == 0);
    let ran_b = workers
        .iter()
        .any(|w| w.id.starts_with("wt-b-") && w.exit == 0);
    assert!(
        ran_a && ran_b,
        "both worktrees must drain through the shared pool"
    );

    println!("scenario 3: SIGKILLed slot holders must not leak slots");
    assert_killed_slots_are_released(tmp.path(), &repo, &payload_bin);
}

/// Build a real git repo and a second linked worktree off its HEAD, so the
/// EC-01 variant's invocations originate from two worktrees of one repo.
fn build_repo_with_second_worktree(repo: &Path, second: &Path) {
    std::fs::create_dir_all(repo).expect("repo dir");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=github@jedarden.com",
        "-c",
        "user.name=jedarden",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "stampede fixture",
    ]);
    git(&[
        "worktree",
        "add",
        "-q",
        second.to_str().expect("utf-8 worktree path"),
        "HEAD",
    ]);
}

/// A permission-preserving copy of this harness binary, to play the payload
/// role (module docs on [`Scenario`]): `fs::copy` keeps the mode bits, so
/// the copy execs like the original.
fn copy_self(payload_bin: &Path) {
    let self_exe = std::env::current_exe().expect("harness executable path");
    std::fs::copy(&self_exe, payload_bin).unwrap_or_else(|e| {
        panic!(
            "cannot copy {} to {}: {e}",
            self_exe.display(),
            payload_bin.display()
        )
    });
}

fn run_scenario(scenario: &Scenario) -> (Vec<WorkerRun>, Vec<(String, i64, i64)>) {
    std::fs::create_dir_all(&scenario.home).expect("scenario HOME");
    std::fs::create_dir_all(&scenario.events).expect("scenario events dir");
    let self_exe = std::env::current_exe().expect("harness executable path");

    // Spawn all invocations back-to-back — simultaneous is the point. Each
    // finishes in well over the spawn loop's total time.
    let children: Vec<_> = scenario
        .workers
        .iter()
        .map(|(id, cwd)| {
            Command::new(&self_exe)
                .arg("--worker")
                .env("STAMPEDE_ID", id)
                .env("STAMPEDE_PAYLOAD", &scenario.payload_bin)
                .env("STAMPEDE_EVENTS", &scenario.events)
                .env("HOME", &scenario.home)
                .env_remove("XDG_STATE_HOME")
                .current_dir(cwd)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn worker {id}: {e}"))
        })
        .collect();

    let mut workers = Vec::with_capacity(children.len());
    for ((id, _), child) in scenario.workers.iter().zip(children) {
        let out = child.wait_with_output().expect("worker wait");
        workers.push(WorkerRun {
            id: id.clone(),
            exit: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let intervals = parse_events(&scenario.events);
    (workers, intervals)
}

/// The per-payload `start`/`end` files → `(id, start, end)` intervals; a
/// missing or malformed file fails here.
fn parse_events(events_dir: &Path) -> Vec<(String, i64, i64)> {
    let mut intervals = Vec::new();
    for entry in std::fs::read_dir(events_dir).expect("events dir exists after the stampede") {
        let path = entry.expect("events dir entry").path();
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let mut start = None;
        let mut end = None;
        let mut id = None;
        for line in raw.lines() {
            let mut parts = line.split_whitespace();
            let phase = parts.next().unwrap_or("");
            let who = parts.next().unwrap_or("");
            let ms = parts.next().and_then(|s| s.parse::<i64>().ok());
            match (phase, who, ms) {
                ("start", who, Some(ms)) => {
                    id = Some(who.to_string());
                    start = Some(ms);
                }
                ("end", who, Some(ms)) => {
                    id = Some(who.to_string());
                    end = Some(ms);
                }
                _ => panic!("malformed event line in {}: {line:?}", path.display()),
            }
        }
        let id = id.unwrap_or_else(|| panic!("empty event file {}", path.display()));
        let start = start.unwrap_or_else(|| panic!("worker {id} never started"));
        let end = end.unwrap_or_else(|| panic!("worker {id} never ended"));
        intervals.push((id, start, end));
    }
    intervals.sort_by_key(|&(_, start, _)| start);
    intervals
}

/// Peak simultaneous payload intervals — an observed lower bound on
/// concurrent slot holders (module docs: a payload runs strictly inside its
/// slot hold). Ends sort before starts at the same millisecond so a
/// handoff instant never overcounts.
fn peak_concurrency(intervals: &[(String, i64, i64)]) -> u32 {
    let mut deltas: Vec<(i64, i32)> = intervals
        .iter()
        .flat_map(|&(_, start, end)| [(start, 1), (end, -1)])
        .collect();
    deltas.sort_unstable();
    let mut live = 0;
    let mut peak = 0;
    for (_, d) in deltas {
        live += d;
        peak = peak.max(live);
    }
    peak as u32
}

fn assert_stampede_outcome(workers: &[WorkerRun], intervals: &[(String, i64, i64)]) {
    // INV-1 / AS-3: every invocation ends in a real verdict. The fallback's
    // verdict is the payload's outcome; the drill payload always exits 0.
    for w in workers {
        assert_eq!(w.exit, 0, "worker {} exited {}", w.id, w.exit);
        assert!(
            w.stderr.contains("[gantry] infra: stampede drill"),
            "worker {} never printed the infra reason:\n{}",
            w.id,
            w.stderr
        );
        assert!(
            w.stderr.contains("[gantry] verdict: Pass"),
            "worker {} never printed its verdict trailer:\n{}",
            w.id,
            w.stderr
        );
        // The bounded wait is a degradation valve, not a service level: with
        // a few-second drain, nobody may hit it.
        assert!(
            !w.stderr.contains("no local slot within"),
            "worker {} hit the bounded wait — the queue wedged:\n{}",
            w.id,
            w.stderr
        );
    }

    assert_eq!(
        intervals.len(),
        workers.len(),
        "every fallback must have run exactly one payload"
    );
    for (id, start, end) in intervals {
        assert!(end > start, "worker {id} payload interval is empty");
    }

    // The cap is real: never more than SLOTS at once (AS-3 fail mode:
    // "simultaneous uncapped local builds — the meltdown gantry exists to
    // prevent").
    let peak = peak_concurrency(intervals);
    assert!(
        peak <= SLOTS,
        "slot cap of {SLOTS} exceeded: peak concurrency {peak}"
    );
    // …and the pool is real: a queue bug that serializes everyone down to a
    // single run (the ticket-lifecycle regression unit-tested in
    // src/local.rs) fails here.
    assert!(
        peak >= 2,
        "pool never filled — peak concurrency {peak} means the semaphore \
         degenerated to a single run"
    );

    // Loud queueing: with SLOTS slots and this many invocations, someone
    // waited, and the wait said so in the plan's exact format (plan §6:
    // `[gantry] waiting for local slot (N ahead)`).
    let queued: Vec<&str> = workers
        .iter()
        .filter(|w| w.stderr.contains("[gantry] waiting for local slot ("))
        .map(|w| w.id.as_str())
        .collect();
    assert!(
        !queued.is_empty(),
        "no worker queued — with {SLOTS} slots and {} invocations, someone \
         must have",
        workers.len()
    );
    assert!(
        workers.iter().any(|w| w.stderr.contains(" ahead)")),
        "a queue-position line must name the position (N ahead)"
    );
    println!(
        "  {} runs, peak concurrency {peak}, {} queued loudly: OK",
        workers.len(),
        queued.len()
    );
}

/// Scenario 3 (module docs): fill every slot, SIGKILL the holders mid-run,
/// and prove fresh probes drain through the pool right away. The kernel
/// releases a dead process's flocks with no gantry code running, so this is
/// the direct test of "SIGKILL-safe = no leaked slots"; a leaked slot fails
/// fast because a probe that cannot get in must print the bounded-wait
/// warning the assertions forbid.
fn assert_killed_slots_are_released(tmp: &Path, repo: &Path, payload_bin: &Path) {
    let home = tmp.join("home-3");
    // Holders and probes share the pool (the point) but get separate event
    // dirs: the holders' payloads outlive their killed workers and finish
    // writing whenever they please, so their files are never parsed.
    let holder_events = tmp.join("events-3-holders");
    let probe_events = tmp.join("events-3-probes");
    std::fs::create_dir_all(&home).expect("scenario 3 HOME");
    std::fs::create_dir_all(&holder_events).expect("holder events dir");
    std::fs::create_dir_all(&probe_events).expect("probe events dir");
    let self_exe = std::env::current_exe().expect("harness executable path");

    // Fill the pool: one holder per slot, each parked far beyond the
    // parent's patience.
    let holders: Vec<(String, std::process::Child)> = (0..SLOTS as u64)
        .map(|i| {
            let id = format!("kill-{i:02}");
            let child = Command::new(&self_exe)
                .arg("--worker")
                .env("STAMPEDE_ID", &id)
                .env("STAMPEDE_PAYLOAD", payload_bin)
                .env("STAMPEDE_EVENTS", &holder_events)
                .env("STAMPEDE_HOLD_MS", KILL_HOLD_MS.to_string())
                .env("HOME", &home)
                .env_remove("XDG_STATE_HOME")
                .current_dir(repo)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn holder {id}: {e}"));
            (id, child)
        })
        .collect();

    // A worker acquires its slot guard *before* spawning its payload, so a
    // `start` line from every holder proves the pool is full.
    let filled = |events: &Path| {
        (0..SLOTS as u64).all(|i| {
            std::fs::read_to_string(events.join(format!("kill-{i:02}.events")))
                .map(|c| c.contains("start "))
                .unwrap_or(false)
        })
    };
    let deadline = Instant::now() + Duration::from_millis(KILL_HOLD_MS);
    while !filled(&holder_events) {
        assert!(
            Instant::now() < deadline,
            "holders never filled the pool — a slot was not acquirable"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    for (id, mut child) in holders {
        // The worker dies with its slot guard alive — exactly the
        // crashed-agent shape. Reaping before the probes start guarantees
        // the kernel has torn the flocks down before they queue.
        child.kill().expect("kill holder");
        let status = child.wait().expect("reap holder");
        assert!(
            !status.success(),
            "holder {id} finished before the SIGKILL landed"
        );
    }

    // Fresh runs must drain through the released pool without touching
    // their (short) bounded wait.
    let probes: Vec<_> = (0..SLOTS as u64)
        .map(|i| {
            let id = format!("probe-{i:02}");
            Command::new(&self_exe)
                .arg("--worker")
                .env("STAMPEDE_ID", &id)
                .env("STAMPEDE_PAYLOAD", payload_bin)
                .env("STAMPEDE_EVENTS", &probe_events)
                .env("STAMPEDE_WAIT_SECS", KILL_PROBE_WAIT_SECS.to_string())
                .env("HOME", &home)
                .env_remove("XDG_STATE_HOME")
                .current_dir(repo)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn probe {id}: {e}"))
        })
        .collect();
    for child in probes {
        let out = child.wait_with_output().expect("probe wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "probe failed:\n{stderr}");
        assert!(
            !stderr.contains("no local slot within"),
            "a slot leaked past SIGKILL — the probe gave up waiting:\n{stderr}"
        );
        assert!(
            stderr.contains("[gantry] verdict: Pass"),
            "probe never reached its verdict:\n{stderr}"
        );
    }
    let intervals = parse_events(&probe_events);
    assert_eq!(
        intervals.len(),
        SLOTS as usize,
        "every probe must have run exactly one payload"
    );

    println!("  {SLOTS} slots freed by SIGKILL, {SLOTS} probes drained: OK");
}
