// gantry — LocalExecutor: the fallback admission semaphore (plan Component 6).
//
// Phase 1b (bf-299). When a remote run degrades to a capped local fallback —
// an infra failure on push, submit, or wait — it must not stampede the box
// together with every other fallback from the same outage. Each fallback run
// first acquires one of a fixed pool of `flock`-held token files (default 3
// concurrent per box), queueing loudly while it waits:
//
//     [gantry] waiting for local slot (4 ahead)
//
// The semaphore is two sets of lock files under the state dir
// (`<state>/fallback-slots/`):
//
// - **Slots** (`slots/slot-<i>`) are the capacity: a running fallback holds
//   `LOCK_EX` on exactly one slot file for the duration of its local run.
//   Acquiring a slot is a single non-blocking `flock` — atomicity lives in
//   the kernel, so two processes can never both believe they hold slot N.
// - **Tickets** (`tickets/ticket-<n>`) are the order: each waiting run mints
//   a monotonic ticket under a brief `queue.lock` and holds that ticket's
//   `LOCK_EX` while it waits. Holding the ticket proves the waiter is alive
//   and keeps every later arrival behind it — strict FIFO, one contender at
//   a time, no thundering herd on a freed slot. The ticket is released the
//   instant its run acquires a slot: from then on the slot flock proves
//   liveness, and a holder that kept its ticket would count as "ahead" of
//   every waiter — serializing the pool down to a single run no matter how
//   many slots it has.
//
// A ticket or slot whose holder dies (including SIGKILL) has its lock
// released by the kernel: the next scanner acquires the dead ticket, sees it
// is unheld, unlinks it (ticket numbers are never reused), and moves on; a
// freed slot is simply acquired by the next head of the queue. A crashed run
// therefore can never wedge the queue or leak a slot — the "kernel lock
// release = no leaked slots" property. Slot files themselves are never
// unlinked: unlinking a held lock file would let a fresh create mint a second
// lock object for the same name and silently raise the capacity.
//
// The wait is bounded (`fallback_wait_secs`, default one hour): past the
// bound the run proceeds WITHOUT a slot. INV-1 — every invocation ends in a
// real verdict — outranks the admission cap, the per-run cgroup cap still
// applies to the unqueued run, and the boxwide `gantry.slice` sum cap
// (bf-xj0) is the total-load backstop. The timeout is loud, never silent.
//
// Degradations, both deliberate:
// - EC-08: if the state dir is unusable, the fallback runs without a slot,
//   loudly. A broken semaphore never blocks the build.
// - Non-Unix has no `flock`; the lock primitives degrade to no-ops there and
//   capacity is not enforced (same posture as R5/Q-5 for systemd caps —
//   degrade with documentation, never block). Ticket minting still
//   deduplicates via create-new, so ordering files stay well-formed.
//
// Scope note: the semaphore bounds *fallback* runs only. Tier-0 and
// kill-switch local runs are the caller's explicit choice of local mode, not
// degradations, and do not queue. The `GANTRY_AGENT` humans-first ordering
// (plan Component 6, bf-xj0) changes only who counts as "head of queue";
// the slot and ticket mechanics here are its substrate.

use crate::config::Config;
use crate::runlog::{Durations, RanLocation, RunLog, Verdict, VerdictRecord};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Poll cadence for the queue: start responsive, back off to a calm idle.
const POLL_MIN: Duration = Duration::from_millis(100);
const POLL_MAX: Duration = Duration::from_millis(1000);

/// Minimum spacing between repeated `[gantry] waiting` lines with an
/// unchanged position. Loud does not mean one line per 100 ms poll.
const QUEUE_LINE_INTERVAL: Duration = Duration::from_secs(10);

// ============================================================================
// Kernel locks
// ============================================================================

/// Open (or create) a lock file for flock use.
fn open_lock_file(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true);
    }
    opts.open(path)
}

/// Try to take `LOCK_EX` on an open lock file without blocking. `true` means
/// acquired — for a ticket file that proves the previous holder is gone.
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock(2) on a valid fd; it touches no memory and fails cleanly
    // with EWOULDBLOCK when the lock is held.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Non-Unix degrade: no flock exists, so "try" always succeeds and capacity
/// is unenforced (module docs, R5 posture).
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> bool {
    true
}

/// Take `LOCK_EX` blocking (used only for the microseconds-long queue lock).
/// Gives up on a persistent non-EINTR error rather than spinning forever: the
/// mint path stays correct without the lock because ticket creation is
/// create-new (module docs, R9).
#[cfg(unix)]
fn lock_exclusive_blocking(file: &File) {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: as in try_lock_exclusive.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Non-Unix degrade for the blocking lock (see [`try_lock_exclusive`]).
#[cfg(not(unix))]
fn lock_exclusive_blocking(_file: &File) {}

// ============================================================================
// Semaphore
// ============================================================================

/// The fallback admission semaphore (plan Component 6).
///
/// Construct with [`FallbackSemaphore::open`] (state dir from config) or
/// [`FallbackSemaphore::open_with`] (explicit dir, for tests). A single
/// instance may be shared across threads; every [`Self::admit`] call is an
/// independent acquisition.
pub struct FallbackSemaphore {
    /// `<state>/fallback-slots` — holds `slots/` and `tickets/`.
    dir: PathBuf,
    /// Concurrent fallback runs allowed (config `local.fallback_slots`).
    slots: u32,
    /// Bounded wait for a slot (config `local.fallback_wait_secs`).
    max_wait: Duration,
}

/// Outcome of [`FallbackSemaphore::admit`].
#[derive(Debug)]
pub enum Admission {
    /// A slot is held; the guard's lifetime is the slot's. Drop it — or die,
    /// by any means including SIGKILL — and the kernel releases the slot.
    Acquired(SlotGuard),
    /// The bounded wait expired with no slot free. The caller decides how to
    /// proceed; the fallback path runs without a slot (module docs).
    TimedOut,
}

/// Error setting the semaphore up. Per EC-08 the fallback path treats every
/// variant the same way: warn loudly and run without a slot.
#[derive(Debug)]
pub enum SemaphoreError {
    /// No state directory could be determined (no HOME).
    NoStateDir,
    /// A filesystem operation on the semaphore directory failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for SemaphoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SemaphoreError::NoStateDir => {
                write!(f, "cannot determine state directory")
            }
            SemaphoreError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for SemaphoreError {}

/// One ticket in the FIFO: its sequence number and the lock proving the
/// waiter is alive. The lock is held from mint until the run acquires a slot
/// (then dropped — the slot lock takes over as the liveness proof) or gives
/// up its bounded wait.
struct Ticket {
    number: u64,
    /// Open file description carrying the exclusive flock. Dropping it
    /// releases the lock; the process dying releases it in the kernel.
    _lock: File,
}

/// A held fallback slot. Dropping the guard releases the slot lock by
/// closing the file description; process death (any signal, SIGKILL
/// included) releases it in the kernel with no gantry code running.
#[derive(Debug)]
pub struct SlotGuard {
    /// The slot file whose `LOCK_EX` this run holds. Named for the linter,
    /// not for reading: the field's entire job is to stay open — dropping it
    /// releases the lock, here or at process death.
    _slot: File,
}

impl FallbackSemaphore {
    /// Build a semaphore under the gantry state dir from config.
    ///
    /// Reads `local.fallback_slots` and `local.fallback_wait_secs` and creates
    /// the semaphore directory if needed. Errors per EC-08 are reported to the
    /// caller, which degrades to an unqueued run.
    pub fn open(config: &Config) -> Result<Self, SemaphoreError> {
        let state_dir = crate::state::StateFile::state_dir().ok_or(SemaphoreError::NoStateDir)?;
        Self::open_with(
            state_dir.join("fallback-slots"),
            config.local.fallback_slots,
            Duration::from_secs(config.local.fallback_wait_secs),
        )
    }

    /// Build a semaphore at an explicit directory with explicit limits.
    ///
    /// Tests use this to point the pool at a temp dir without touching the
    /// process environment. A zero slot count is floored at 1 — a zero-slot
    /// semaphore could never admit anyone.
    pub fn open_with(dir: PathBuf, slots: u32, max_wait: Duration) -> Result<Self, SemaphoreError> {
        io_map(fs::create_dir_all(&dir), &dir)?;
        io_map(fs::create_dir_all(dir.join("slots")), &dir.join("slots"))?;
        Ok(Self {
            dir,
            slots: slots.max(1),
            max_wait,
        })
    }

    /// Acquire a fallback slot, queueing loudly (FIFO) for at most
    /// `max_wait`.
    ///
    /// The queue position line prints on the first wait and whenever the
    /// position changes (plus at most once per [`QUEUE_LINE_INTERVAL`] while
    /// nothing changes) so an agent transcript sees movement without per-poll
    /// spam.
    pub fn admit(&self) -> Result<Admission, SemaphoreError> {
        let deadline = Instant::now() + self.max_wait;
        let ticket = self.mint_ticket()?;
        let mut announced: Option<(u64, Instant)> = None;
        let mut poll = POLL_MIN;

        loop {
            let ahead = self.live_tickets_below(ticket.number)?;

            // Only the smallest live ticket may try slots — strict FIFO, and
            // (module docs) exactly one contender exists at any instant, so
            // the non-blocking slot flock below has no competitor to lose to.
            if ahead == 0 {
                if let Some(slot) = self.try_slots()? {
                    // The slot flock is now this run's liveness proof; the
                    // ticket's ordering job is done. Release it at once —
                    // kept, it would sit "ahead" of every waiter (its number
                    // is the smallest live one) and hold the whole queue
                    // behind a run that already has its slot, collapsing the
                    // pool to a single concurrent run.
                    drop(ticket);
                    return Ok(Admission::Acquired(SlotGuard { _slot: slot }));
                }
            }

            Self::announce_queue_position(ahead, &mut announced);

            if Instant::now() >= deadline {
                // Drop the ticket (releasing its lock) on the way out so the
                // queue does not wait on a waiter that already gave up.
                drop(ticket);
                return Ok(Admission::TimedOut);
            }

            std::thread::sleep(poll);
            poll = (poll * 2).min(POLL_MAX);
        }
    }

    /// Print the loud queue-position line, throttled by position change or
    /// [`QUEUE_LINE_INTERVAL`], whichever comes first.
    fn announce_queue_position(ahead: u64, announced: &mut Option<(u64, Instant)>) {
        let now = Instant::now();
        let due = match *announced {
            None => true,
            Some((last_ahead, at)) => {
                last_ahead != ahead || now.duration_since(at) >= QUEUE_LINE_INTERVAL
            }
        };
        if due {
            eprintln!("{}", queue_line(ahead));
            *announced = Some((ahead, now));
        }
    }

    /// Mint the next FIFO ticket: bump the counter under the queue lock,
    /// create the ticket file create-new (collision-proof even where the lock
    /// degraded), and hold its exclusive lock from this instant.
    fn mint_ticket(&self) -> Result<Ticket, SemaphoreError> {
        let tickets_dir = self.dir.join("tickets");
        io_map(fs::create_dir_all(&tickets_dir), &tickets_dir)?;

        let queue_lock_path = self.dir.join("queue.lock");
        let queue_lock =
            open_lock_file(&queue_lock_path, false).map_err(|e| SemaphoreError::Io {
                path: queue_lock_path.clone(),
                source: e,
            })?;
        lock_exclusive_blocking(&queue_lock);

        let mut number = read_counter(&tickets_dir) + 1;
        let (file, path) = loop {
            let path = tickets_dir.join(format!("ticket-{number:020}"));
            match open_lock_file(&path, true) {
                Ok(file) => break (file, path),
                // Number already taken (a degraded-lock race, or a replayed
                // counter): skip to the next number rather than clobber.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    number += 1;
                }
                Err(e) => {
                    return Err(SemaphoreError::Io { path, source: e });
                }
            }
        };

        write_counter(&tickets_dir, number)?;
        drop(queue_lock);

        // We just created the file, so the lock cannot already be held —
        // failure here would mean the world changed underneath us; fail
        // loudly rather than queue out of order.
        if !try_lock_exclusive(&file) {
            return Err(SemaphoreError::Io {
                path,
                // `io::Error::other` is 1.74+ and this crate's MSRV is 1.70.
                source: std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "freshly minted ticket is already locked",
                ),
            });
        }

        Ok(Ticket {
            number,
            _lock: file,
        })
    }

    /// Count live tickets strictly below `number` — the honest queue position,
    /// since FIFO means every one of them starts before this run does. Dead
    /// tickets found along the way (lock acquirable = holder gone) are
    /// unlinked; numbers are never reused, so this cannot race a live holder.
    fn live_tickets_below(&self, number: u64) -> Result<u64, SemaphoreError> {
        let tickets_dir = self.dir.join("tickets");
        let mut live = 0u64;
        for entry in fs::read_dir(&tickets_dir).map_err(|e| SemaphoreError::Io {
            path: tickets_dir.clone(),
            source: e,
        })? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue, // a vanished entry is a dead ticket
            };
            let Some(n) = parse_ticket_name(&entry.file_name()) else {
                continue;
            };
            if n >= number {
                continue;
            }
            let path = tickets_dir.join(entry.file_name());
            let file = match open_lock_file(&path, false) {
                Ok(f) => f,
                // Vanished (another scanner cleaned it): gone is dead.
                Err(_) => continue,
            };
            if try_lock_exclusive(&file) {
                // Acquired, so no one holds it: the holder died. Releasing
                // `file` (drop) plus unlinking keeps the dir bounded.
                drop(file);
                let _ = fs::remove_file(&path);
            } else {
                live += 1;
            }
        }
        Ok(live)
    }

    /// Try to claim a free slot (non-blocking). Returns the open, locked file
    /// on success. Slot files are created on demand (config raised the count)
    /// and never unlinked (module docs).
    fn try_slots(&self) -> Result<Option<File>, SemaphoreError> {
        let slots_dir = self.dir.join("slots");
        for i in 0..self.slots {
            let path = slots_dir.join(format!("slot-{i}"));
            let file = open_lock_file(&path, false).map_err(|e| SemaphoreError::Io {
                path: path.clone(),
                source: e,
            })?;
            if try_lock_exclusive(&file) {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }
}

/// Fold an `io::Result` into a [`SemaphoreError`] naming the path.
fn io_map(result: std::io::Result<()>, path: &Path) -> Result<(), SemaphoreError> {
    result.map_err(|e| SemaphoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

/// The loud queue-position line (plan Component 6:
/// `[gantry] waiting for local slot (N ahead)`). `ahead` counts live
/// *waiting* tickets below this run's — under FIFO, exactly the queued runs
/// that start first (slot holders have already released their tickets).
fn queue_line(ahead: u64) -> String {
    format!("[gantry] waiting for local slot ({ahead} ahead)")
}

/// Parse `ticket-<n>` back to `n`; anything else (slot files, the counter,
/// strays) is not a ticket.
fn parse_ticket_name(file_name: &std::ffi::OsStr) -> Option<u64> {
    file_name
        .to_str()?
        .strip_prefix("ticket-")?
        .parse::<u64>()
        .ok()
}

/// Read the monotonic ticket counter; unreadable or garbage means 0 (the mint
/// path still cannot collide, because ticket creation is create-new).
fn read_counter(tickets_dir: &Path) -> u64 {
    fs::read_to_string(tickets_dir.join("counter"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Persist the monotonic ticket counter.
fn write_counter(tickets_dir: &Path, value: u64) -> Result<(), SemaphoreError> {
    let path = tickets_dir.join("counter");
    fs::write(&path, value.to_string()).map_err(|e| SemaphoreError::Io { path, source: e })
}

// ============================================================================
// Fallback execution
// ============================================================================

/// Context the remote pipeline hands a fallback so the local run can close
/// the already-written intent with one matching verdict record.
pub struct FallbackContext<'a> {
    /// Open runlog, if the state dir allowed one (EC-08).
    pub runlog: Option<&'a RunLog>,
    /// Run id from the write-ahead intent (INV-1 pairing).
    pub run_id: &'a str,
    /// GitGate duration already spent upstream (ms), for the duration split.
    pub gate_ms: u64,
    /// RefPusher duration already spent upstream (ms).
    pub push_ms: u64,
}

/// Run a degraded local invocation through the admission semaphore — the
/// `InfraFailure` tail of the remote pipeline (plan Component 6, AS-3).
///
/// Prints the infra reason, acquires a fallback slot (queueing loudly, bounded
/// by `local.fallback_wait_secs`), then runs the caller's argv on the real
/// binary — the same resolution rules as the Tier-0 tail (never a
/// fallback-to-self, plan §1) — and closes the intent with one verdict record
/// carrying `ran: local_after_infra` and the queue duration.
///
/// The local outcome *is* the verdict: exit 0 is `Pass`, anything else
/// `TestFailure` — the suite's own failure is not an infra failure. A binary
/// that cannot be resolved or spawned is a recorded, non-zero `InfraFailure`.
///
/// Nothing here can fail the run on its own: an unusable semaphore (EC-08) or
/// an expired bounded wait proceeds without a slot, loudly, because INV-1
/// outranks the cap (module docs).
pub fn run_fallback(
    config: &Config,
    args: &[String],
    infra_reason: &str,
    ctx: &FallbackContext<'_>,
) -> i32 {
    // Repo URL and sha need no re-passing: the intent written upstream already
    // carries them, and the verdict record below closes that same run id.
    eprintln!("[gantry] infra: {infra_reason}");
    eprintln!("[gantry] falling back to capped local run");

    // Acquire a slot. Every degradation here is loud and runs anyway: a
    // broken semaphore (EC-08) or a full bounded wait must not cost the
    // caller its verdict.
    let semaphore = match FallbackSemaphore::open(config) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "[gantry] warning: fallback semaphore unavailable ({}); \
                 running without a slot",
                e
            );
            None
        }
    };

    let queue_start = Instant::now();
    let mut _slot_guard = None;
    if let Some(semaphore) = &semaphore {
        match semaphore.admit() {
            Ok(Admission::Acquired(guard)) => _slot_guard = Some(guard),
            Ok(Admission::TimedOut) => eprintln!(
                "[gantry] warning: no local slot within {}s; running without \
                 a slot (per-run cap still applies)",
                config.local.fallback_wait_secs
            ),
            Err(e) => eprintln!(
                "[gantry] warning: fallback semaphore unavailable ({}); \
                 running without a slot",
                e
            ),
        }
    }
    let queue_ms = queue_start.elapsed().as_millis() as u64;

    // Same resolution contract as every local tail: the shim's rules, never
    // a fallback-to-self (plan §1).
    let run_start = Instant::now();
    let real = match crate::shim::resolve_real_binary(config) {
        Ok(path) => path,
        Err(why) => {
            eprintln!("[gantry] {why}");
            record_fallback_verdict(
                ctx,
                Verdict::InfraFailure,
                1,
                queue_ms,
                run_start.elapsed().as_millis() as u64,
            );
            eprintln!("[gantry] verdict: InfraFailure");
            return 1;
        }
    };

    let status = std::process::Command::new(&real).args(args).status();

    let (verdict, exit_code) = match status {
        Ok(status) => {
            let code = status_to_i32(status);
            let verdict = if code == 0 {
                Verdict::Pass
            } else {
                Verdict::TestFailure
            };
            (verdict, code)
        }
        Err(why) => {
            eprintln!("[gantry] failed to run `{}`: {why}", real.display());
            (Verdict::InfraFailure, 1)
        }
    };

    record_fallback_verdict(
        ctx,
        verdict,
        exit_code,
        queue_ms,
        run_start.elapsed().as_millis() as u64,
    );

    eprintln!("[gantry] verdict: {verdict}");
    exit_code
}

/// Close the write-ahead intent with the fallback's terminal record:
/// `ran: local_after_infra`, the queue duration included in the split. The
/// remote attempt's timings are upstream's; the record schema has one slot
/// per stage and the flight recorder (bf-3mc) is the full-trace home.
fn record_fallback_verdict(
    ctx: &FallbackContext<'_>,
    verdict: Verdict,
    exit_code: i32,
    queue_ms: u64,
    run_ms: u64,
) {
    if let Some(rl) = ctx.runlog {
        let record = VerdictRecord::new(
            ctx.run_id.to_string(),
            verdict,
            RanLocation::LocalAfterInfra,
            exit_code,
            "local".to_string(),
            Some(Durations {
                gate: ctx.gate_ms,
                push: ctx.push_ms,
                queue: queue_ms,
                run: run_ms,
            }),
        );
        if let Err(e) = rl.close_verdict(&record) {
            eprintln!("[gantry] warning: cannot write verdict record: {}", e);
        }
    }
}

/// Map a child's exit status to a faithful i32 (INV-3) — success → 0, a
/// normal exit code verbatim, Unix death-by-signal S → 128+S (the shell's
/// `$?` convention).
fn status_to_i32(status: std::process::ExitStatus) -> i32 {
    if status.success() {
        return 0;
    }
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn semaphore(dir: PathBuf, slots: u32, wait_ms: u64) -> FallbackSemaphore {
        FallbackSemaphore::open_with(dir, slots, Duration::from_millis(wait_ms)).unwrap()
    }

    #[test]
    fn queue_line_matches_plan_format() {
        assert_eq!(queue_line(0), "[gantry] waiting for local slot (0 ahead)");
        assert_eq!(queue_line(17), "[gantry] waiting for local slot (17 ahead)");
    }

    #[test]
    fn ticket_names_parse_and_reject_strays() {
        assert_eq!(
            parse_ticket_name(OsStr::new("ticket-00000000000000000042")),
            Some(42)
        );
        assert_eq!(parse_ticket_name(OsStr::new("ticket-7")), Some(7));
        assert_eq!(parse_ticket_name(OsStr::new("ticket-")), None);
        assert_eq!(parse_ticket_name(OsStr::new("ticket-x")), None);
        assert_eq!(parse_ticket_name(OsStr::new("slot-1")), None);
        assert_eq!(parse_ticket_name(OsStr::new("counter")), None);
    }

    #[test]
    fn garbage_counter_reads_as_zero() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("counter"), "not-a-number\n").unwrap();
        assert_eq!(read_counter(dir.path()), 0);
        assert_eq!(read_counter(&dir.path().join("nonexistent")), 0);
    }

    #[test]
    fn zero_slot_count_floors_at_one() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 0, 1000);
        assert_eq!(sem.slots, 1);
    }

    #[test]
    fn state_dir_error_is_displayable() {
        let e = SemaphoreError::NoStateDir;
        assert_eq!(e.to_string(), "cannot determine state directory");
    }

    /// The cap under contention: with 2 slots, 6 concurrent acquirers must
    /// overlap in both directions — never more than 2 at any instant (the
    /// cap is real), and genuinely 2 at the peak (the pool actually fills;
    /// a queue bug that serializes everyone to one run fails here).
    #[test]
    #[cfg(unix)]
    fn cap_holds_under_thread_contention() {
        let dir = TempDir::new().unwrap();
        let sem = Arc::new(semaphore(dir.path().to_path_buf(), 2, 30_000));
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(Mutex::new(0usize));
        let acquired = Arc::new(AtomicUsize::new(0));

        let workers: Vec<_> = (0..6)
            .map(|_| {
                let sem = Arc::clone(&sem);
                let live = Arc::clone(&live);
                let max_live = Arc::clone(&max_live);
                let acquired = Arc::clone(&acquired);
                std::thread::spawn(move || match sem.admit().unwrap() {
                    Admission::Acquired(_guard) => {
                        acquired.fetch_add(1, Ordering::SeqCst);
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        {
                            let mut m = max_live.lock().unwrap();
                            *m = (*m).max(now);
                        }
                        std::thread::sleep(Duration::from_millis(150));
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                    Admission::TimedOut => panic!("30s budget expired for a 6-run storm"),
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }

        assert_eq!(acquired.load(Ordering::SeqCst), 6, "every run got a slot");
        let peak = *max_live.lock().unwrap();
        assert!(peak <= 2, "cap of 2 slots was exceeded (peak {peak})");
        assert!(peak >= 2, "pool never filled: peak concurrency {peak}");
    }

    /// Deterministic capacity check: on a 3-slot pool three sequential
    /// admissions must all succeed concurrently, and only the fourth — with
    /// every slot still held — must time out. This is the regression test
    /// for the ticket-lifecycle bug where slot holders kept their FIFO
    /// ticket: then, the second admission already hung ("1 ahead") and the
    /// pool silently degraded to capacity 1.
    #[test]
    #[cfg(unix)]
    fn three_slot_pool_admits_three_and_blocks_the_fourth() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 3, 1_000);

        let g1 = match sem.admit().unwrap() {
            Admission::Acquired(g) => g,
            Admission::TimedOut => panic!("first admission on an empty 3-slot pool"),
        };
        let g2 = match sem.admit().unwrap() {
            Admission::Acquired(g) => g,
            Admission::TimedOut => panic!("second admission: a holder is not ahead of you"),
        };
        let g3 = match sem.admit().unwrap() {
            Admission::Acquired(g) => g,
            Admission::TimedOut => panic!("third admission: the pool holds three"),
        };

        let fourth = semaphore(dir.path().to_path_buf(), 3, 250);
        assert!(
            matches!(fourth.admit().unwrap(), Admission::TimedOut),
            "a fourth run must wait: all three slots are held"
        );

        // Release one and the pool admits again.
        drop(g1);
        match sem.admit().unwrap() {
            Admission::Acquired(_) => (),
            Admission::TimedOut => panic!("a released slot must be acquirable"),
        }
        drop(g2);
        drop(g3);
    }

    /// The bounded wait: with the only slot held elsewhere, a waiter must
    /// come back TimedOut after roughly its budget — not hang forever.
    #[test]
    #[cfg(unix)]
    fn bounded_wait_times_out_when_slot_never_frees() {
        let dir = TempDir::new().unwrap();
        let sem = Arc::new(semaphore(dir.path().to_path_buf(), 1, 300));
        let holder = sem.admit().unwrap();
        assert!(matches!(holder, Admission::Acquired(_)));

        let started = Instant::now();
        let outcome = sem.admit().unwrap();
        assert!(matches!(outcome, Admission::TimedOut), "expected TimedOut");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "gave up before the budget elapsed"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded wait was not bounded"
        );
    }

    /// No leaked slot in-process: dropping the guard (the graceful version of
    /// process death) must let the next run acquire immediately.
    #[test]
    #[cfg(unix)]
    fn dropped_guard_releases_the_slot() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 1, 2_000);

        let guard = match sem.admit().unwrap() {
            Admission::Acquired(g) => g,
            Admission::TimedOut => panic!("fresh semaphore must admit"),
        };
        drop(guard);

        match sem.admit().unwrap() {
            Admission::Acquired(_) => (), // released — nothing leaked
            Admission::TimedOut => panic!("slot leaked after guard drop"),
        }
    }

    /// Dead-ticket hygiene: a pre-existing unlocked ticket (its holder died)
    /// must not wedge the queue, must be counted as nobody-ahead, and is
    /// cleaned up by the next passer-by.
    #[test]
    #[cfg(unix)]
    fn dead_tickets_are_skipped_and_cleaned() {
        let dir = TempDir::new().unwrap();
        let tickets = dir.path().join("tickets");
        fs::create_dir_all(&tickets).unwrap();

        // A stale holder-less ticket plus a counter already past it.
        let stale = tickets.join(format!("ticket-{:020}", 5));
        fs::write(&stale, "").unwrap();
        fs::write(tickets.join("counter"), "10").unwrap();

        let sem = semaphore(dir.path().to_path_buf(), 1, 1_000);
        assert!(matches!(sem.admit().unwrap(), Admission::Acquired(_)));
        assert!(!stale.exists(), "dead ticket was not cleaned up");

        // The next mint continues past the stale number (the admit above
        // consumed 11, so the counter is already there).
        let next = sem.mint_ticket().unwrap();
        assert_eq!(next.number, 12);
    }

    /// FIFO position counts *waiting* tickets only: a live earlier waiter is
    /// "1 ahead"; that waiter dying (ticket released) must unblock the queue
    /// rather than leave a phantom ahead-count. Slot holders are never
    /// counted — they released their tickets the moment they got their slot.
    #[test]
    #[cfg(unix)]
    fn queue_position_counts_live_waiting_tickets_only() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 1, 5_000);

        let earlier = sem.mint_ticket().unwrap();
        let later = sem.mint_ticket().unwrap();

        assert_eq!(
            sem.live_tickets_below(later.number).unwrap(),
            1,
            "the live earlier waiter is ahead"
        );
        assert_eq!(
            sem.live_tickets_below(earlier.number).unwrap(),
            0,
            "nobody is ahead of the head waiter"
        );

        // The earlier waiter gives up (bounded wait, crash — same kernel
        // effect): the next scan must see the queue clear, not wedge on the
        // corpse.
        drop(earlier);
        assert_eq!(
            sem.live_tickets_below(later.number).unwrap(),
            0,
            "a released ticket must stop counting as ahead"
        );
        drop(later);
    }
}
