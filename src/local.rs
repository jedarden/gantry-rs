// gantry — LocalExecutor: slice placement, scope caps, the fallback admission
// semaphore, and agent-priority queueing (plan Component 6).
//
// Phase 1b (bf-299, bf-xj0). When a remote run degrades to a capped local
// fallback — an infra failure on push, submit, or wait — it must not stampede
// the box together with every other fallback from the same outage. Each
// fallback run first acquires one of a fixed pool of `flock`-held token files
// (default 3 concurrent per box), queueing loudly while it waits:
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
//   and keeps every later arrival behind it — FIFO, one contender per class
//   at a time, no thundering herd on a freed slot. The ticket is released the
//   instant its run acquires a slot: from then on the slot flock proves
//   liveness, and a holder that kept its ticket would count as "ahead" of
//   every waiter — serializing the pool down to a single run no matter how
//   many slots it has.
//
// **Priority (bf-xj0):** `GANTRY_AGENT=1` marks an invocation as an agent
// (the env convention, deliberately not TTY sniffing — tmux-hosted agents
// have TTYs), and agents queue *behind* interactive runs: an interactive
// waiter counts only live interactive tickets as "ahead", while an agent
// counts every live ticket. Humans first, then agents in FIFO order among
// themselves, with a triage tag on the agent's queue line. Continuous
// interactive arrivals can hold an agent behind them indefinitely — the
// bounded wait is the tripwire: past `fallback_wait_secs` the agent proceeds
// without a slot, loudly, because INV-1 (a real verdict) outranks the cap.
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
// applies to the unqueued run, and the boxwide `gantry.slice` sum cap is the
// total-load backstop. The timeout is loud, never silent.
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
// degradations, and do not queue. Slice placement (`gantry.slice`, below)
// applies to gantry's two capped local tails — the Tier-0/local-mode
// execution (decision.rs `execute_locally`) and the infra fallback
// (`run_fallback` here). The passthrough fast path (`GANTRY_LOCAL=1`, and
// every non-intercepted invocation) stays a plain spawn: its INV-4 budget is
// under 5 ms — no room for the manager probe — and an explicit
// `GANTRY_LOCAL=1` is the operator bypassing gantry, not asking to be
// capped by it.
//
// ----------------------------------------------------------------------------
//
// The boxwide slice (bf-xj0): every gantry-spawned local run lands in one
// systemd user slice, `gantry.slice`, carrying the box-level *sum* cap
// (`local.slice_cpu_quota_pct` / `local.slice_memory_max`, default 12 CPU /
// 32G) alongside the per-run caps — the semaphore bounds the count of
// fallbacks, the per-run scope properties bound each run, and the slice
// bounds the total, so 20 × "200%/6G each" can no longer add up past the
// machine. Gantry provisions the slice unit itself (idempotently, under the
// user's systemd config dir, then `daemon-reload`) and launches every local
// run through:
//
//     systemd-run --scope --user --slice=gantry.slice \
//         -p CPUQuota={cpu_quota_pct}% -p MemoryMax={memory_max} \
//         -p MemorySwapMax=0 -q -- <real> <args…>
//
// (the plan §6 capping line plus `--slice`). `--scope` keeps the child a
// direct descendant whose exit status systemd-run propagates, so exit-code
// fidelity (INV-3) survives the wrapper.
//
// Degrade is clean and loud, once per process: no systemd (macOS), no user
// manager (containers, CI), or a slice unit the manager cannot see — gantry
// verifies the provisioned unit is actually loaded before trusting it — and
// every local run falls back to a plain spawn with a single `[gantry] note:`
// line. Risk R5 posture: degrade with documentation, never block; the
// semaphore and the bounded wait still apply on the fallback path.
//
// Known sharp edge, documented rather than hidden: `systemd-run --scope`
// reports both "suite failed" and "scope could not be created" as a non-zero
// exit. The one-time probe plus the loaded-unit verification make that a
// mid-flight rarity (manager restart between probe and spawn); a plain-spawn
// re-try on failure would double-run real suites, so it is deliberately not
// attempted.

use crate::config::Config;
use crate::runlog::{Durations, RanLocation, RunLog, Verdict, VerdictRecord};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
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
// Queue priority (plan Component 6: GANTRY_AGENT humans-first ordering)
// ============================================================================

/// Which side of the humans-first ordering a run queues on.
///
/// `GANTRY_AGENT=1` (the env convention — deliberately not TTY sniffing,
/// since tmux-hosted agents have TTYs) marks an invocation as an agent.
/// Every future submission gate inherits the same two-class ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueClass {
    /// A human is waiting. Never queued behind an agent.
    Interactive,
    /// A machine is waiting (`GANTRY_AGENT=1`). Queued behind every live
    /// ticket — humans first — but FIFO among agents.
    Agent,
}

impl QueueClass {
    /// Classify an invocation from the `GANTRY_AGENT` environment variable.
    ///
    /// `1` (and the conventional truthy spellings, case-insensitive) mark an
    /// agent; unset, empty, or anything else is interactive — a stray value
    /// must never demote a human behind machines.
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var("GANTRY_AGENT").ok().as_deref())
    }

    /// [`Self::from_env`] over a supplied value — the pure, testable core.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on") => {
                QueueClass::Agent
            }
            _ => QueueClass::Interactive,
        }
    }

    /// The marker minted into a ticket file so another waiter — which cannot
    /// see this process's environment — can classify it.
    fn marker(self) -> &'static str {
        match self {
            QueueClass::Interactive => "interactive",
            QueueClass::Agent => "agent",
        }
    }

    /// Recover the class written by [`Self::marker`]. Unreadable or
    /// unrecognized content (a pre-priority ticket, a truncated write) reads
    /// as [`QueueClass::Interactive`]: the conservative reading keeps the
    /// pre-priority FIFO behavior for anything this version did not mint.
    fn from_marker(text: &str) -> Self {
        // Exact match: the minted marker is the bare word (no padding, no
        // newline — `write_all(marker())`), so anything else, whitespace
        // included, is a damaged write and reads conservative-interactive.
        match text {
            "agent" => QueueClass::Agent,
            _ => QueueClass::Interactive,
        }
    }
}

// ============================================================================
// Boxwide slice placement (plan Component 6)
// ============================================================================

/// The single systemd user slice every gantry-spawned local run lands in
/// (plan Component 6). The slice unit carries the box-level *sum* cap —
/// fixed at 12 CPU / 32G until the `local.slice_*` tuning keys land with
/// the slice-config surface — so the total load of all gantry runs stays
/// inside it no matter how many per-run scopes nest beneath.
pub const SLICE_NAME: &str = "gantry.slice";

/// How the next local child is spawned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Wrap the child in `systemd-run --scope --user --slice=gantry.slice …`:
    /// the per-run caps (`local.cpu_quota_pct` / `local.memory_max` /
    /// `MemorySwapMax=0`) apply to the scope, and the slice's sum cap bounds
    /// the total across every concurrent gantry run.
    Slice,
    /// Plain spawn — the slice is disabled, unavailable (no systemd, no user
    /// manager), or its unit would not load (module docs, risk R5).
    Plain,
}

/// The slice unit file for a given box-level sum cap (pure, pinned by test).
///
/// Resource-control directives only — a slice has no process of its own.
/// Accounting is switched on explicitly so the caps are enforced, not merely
/// observed; the leading comment marks the file gantry-managed so a human
/// reading `~/.config/systemd/user/` knows where it came from and that
/// rewriting it is safe (the next gantry run rewrites it back).
fn slice_unit_content(cpu_quota_pct: u32, memory_max: &str) -> String {
    format!(
        "# Managed by gantry — rewritten when the configured cap changes.\n\
         [Slice]\n\
         CPUAccounting=true\n\
         MemoryAccounting=true\n\
         CPUQuota={cpu_quota_pct}%\n\
         MemoryMax={memory_max}\n"
    )
}

/// Build the plan §6 launch line — the per-run capping command extended with
/// `--slice=gantry.slice` (pure, pinned by test):
///
/// ```text
/// systemd-run --scope --user --slice=gantry.slice \
///     -p CPUQuota={per-run}% -p MemoryMax={per-run} -p MemorySwapMax=0 -q -- \
///     <real> <args…>
/// ```
///
/// `--scope` (not `--exec`/a transient service) keeps the child a direct
/// descendant whose exit status systemd-run waits for and propagates, so
/// exit-code fidelity (INV-3) survives the wrapper. `-q` drops systemd-run's
/// own "Running scope as unit…" chatter so the transcript carries gantry's
/// lines, not the wrapper's. argv travels as an array end to end — never
/// shell-interpolated (S-4, INV-5).
fn slice_launch_command(
    systemd_run: &Path,
    real: &Path,
    args: &[String],
    cpu_quota_pct: u32,
    memory_max: &str,
) -> Command {
    let mut cmd = Command::new(systemd_run);
    cmd.arg("--scope")
        .arg("--user")
        .arg(format!("--slice={SLICE_NAME}"))
        .args([
            "-p".to_string(),
            format!("CPUQuota={cpu_quota_pct}%"),
            "-p".to_string(),
            format!("MemoryMax={memory_max}"),
            "-p".to_string(),
            "MemorySwapMax=0".to_string(),
            "-q".to_string(),
            "--".to_string(),
        ])
        .arg(real)
        .args(args);
    cmd
}

/// The user systemd unit directory (`$XDG_CONFIG_HOME/systemd/user`, default
/// `~/.config/systemd/user`) — where the slice unit is provisioned.
fn user_unit_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("systemd").join("user"))
}

/// Find an executable `name` in a PATH-style string. Pure over the supplied
/// value: the production caller passes `$PATH`, tests pass their own — no
/// process-environment mutation in tests.
#[cfg(unix)]
fn find_executable_in_path(path_var: &str, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn find_executable_in_path(path_var: &str, name: &str) -> Option<PathBuf> {
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if fs::metadata(&candidate)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Some(candidate);
        }
    }
    None
}

/// Install the slice unit iff its content differs from what is on disk — a
/// warm box never rewrites the file, so it never pays the daemon-reload that
/// a write requires. Returns whether the file was (re)written.
fn write_unit_if_changed(unit_dir: &Path, content: &str) -> std::io::Result<bool> {
    let unit_path = unit_dir.join(SLICE_NAME);
    if fs::read_to_string(&unit_path).unwrap_or_default() == content {
        return Ok(false);
    }
    fs::create_dir_all(unit_dir)?;
    fs::write(&unit_path, content)?;
    Ok(true)
}

/// One `systemctl --user <args>` invocation: `Ok(())` on success, the trimmed
/// stderr in the error otherwise (loud-degrade material).
fn systemctl_user(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl --user {} failed: {e}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "systemctl --user {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Ask the user manager whether [`SLICE_NAME`] is actually loaded — the
/// loaded-unit verification that makes provisioning trustworthy: a unit file
/// the manager cannot see (a `systemd` user session that reads a different
/// config home, an isolated-HOME test sandbox) would silently cap nothing.
/// `None` when the manager itself could not be asked.
fn slice_load_state() -> Option<String> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            SLICE_NAME,
            "--property=LoadState",
            "--value",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Provision the slice for the given unit content: write the unit if it
/// changed, daemon-reload if it was written, then verify the manager actually
/// loaded it. Every failure mode returns a specific, printable reason — the
/// caller turns it into the one-per-process degrade note.
fn provision_slice(content: &str) -> Result<(), String> {
    let unit_dir = user_unit_dir()
        .ok_or_else(|| "no config directory to install gantry.slice into".to_string())?;
    let wrote = write_unit_if_changed(&unit_dir, content)
        .map_err(|e| format!("cannot write {}: {e}", unit_dir.join(SLICE_NAME).display()))?;
    if wrote {
        systemctl_user(&["daemon-reload"])?;
    }
    match slice_load_state() {
        Some(state) if state == "loaded" => Ok(()),
        Some(state) => Err(format!(
            "{SLICE_NAME} is installed but the user manager reports LoadState={state}"
        )),
        None => Err("no reachable systemd user manager (systemctl --user failed)".to_string()),
    }
}

/// The placement decision given whether the slice is enabled and whether
/// provisioning succeeded (pure — every branch pinned by test). The `Err`
/// case carries the degrade reason, printed once per process as a
/// `[gantry] note:` line; an explicitly disabled slice is a deliberate
/// operator choice, so it degrades silently.
fn decide_placement(enabled: bool, provision: Result<(), String>) -> (Placement, Option<String>) {
    match (enabled, provision) {
        (false, _) => (Placement::Plain, None),
        (true, Ok(())) => (Placement::Slice, None),
        (true, Err(why)) => (
            Placement::Plain,
            Some(format!(
                "gantry.slice unavailable ({why}) — local runs spawn plain, \
                 without cgroup caps; the fallback semaphore still bounds \
                 degraded runs"
            )),
        ),
    }
}

/// One placement decision per process, made lazily before the first local
/// spawn and cached: the probe cost (a PATH scan, at most one unit write, one
/// `daemon-reload`, one load-state query) is paid once, not per run.
#[derive(Debug)]
struct Decision {
    placement: Placement,
    /// The resolved `systemd-run` binary — `Some` exactly when `placement`
    /// is [`Placement::Slice`] (the path search already succeeded).
    systemd_run: Option<PathBuf>,
}

/// Per-run + boxwide slice placement for gantry-spawned local runs
/// (plan Component 6). Build with [`Self::from_config`]; spawn children with
/// [`Self::spawn`], which returns the child's [`ExitStatus`] unchanged — the
/// systemd-run wrapper (when active) waits for the child and propagates its
/// exit code, so every call site keeps its existing INV-3 mapping.
/// Boxwide `gantry.slice` sum-cap defaults (plan Component 6): 12 CPU / 32G.
/// These mirror the `crate::config::DEFAULT_SLICE_*` surface, which lands
/// with the slice-config slice; until then the sum cap is fixed here and the
/// placement cannot be switched off via config.
const DEFAULT_SLICE_CPU_QUOTA_PCT: u32 = 1200;
const DEFAULT_SLICE_MEMORY_MAX: &str = "32G";

pub struct SlicePlacement {
    enabled: bool,
    cpu_quota_pct: u32,
    memory_max: String,
    slice_cpu_quota_pct: u32,
    slice_memory_max: String,
    decision: std::sync::OnceLock<Decision>,
}

impl SlicePlacement {
    /// Read the placement configuration from the layered config.
    pub fn from_config(config: &Config) -> Self {
        Self {
            // `local.slice_enabled` (the operator opt-out) and the
            // `local.slice_*` tuning keys land with the slice-config
            // surface; until then the placement is always on at the default
            // sum cap, and `decide` degrades loudly when systemd-run is
            // unavailable.
            enabled: true,
            cpu_quota_pct: u32::from(config.local.cpu_quota_pct),
            memory_max: config.local.memory_max.clone(),
            slice_cpu_quota_pct: DEFAULT_SLICE_CPU_QUOTA_PCT,
            slice_memory_max: DEFAULT_SLICE_MEMORY_MAX.to_string(),
            decision: std::sync::OnceLock::new(),
        }
    }

    /// Decide (once per process) how local children are spawned.
    fn decide(&self) -> &Decision {
        self.decision.get_or_init(|| {
            let systemd_run =
                find_executable_in_path(&std::env::var("PATH").unwrap_or_default(), "systemd-run");
            // An explicitly disabled slice is the operator opting out: no unit
            // is written and no manager is touched. Otherwise provisioning
            // happens iff the launcher exists — the per-run caps ride the
            // same launch line, so the unit content is provisioned from the
            // live config values.
            let provision = if !self.enabled {
                Ok(())
            } else {
                match systemd_run.as_deref() {
                    Some(_) => provision_slice(&slice_unit_content(
                        self.slice_cpu_quota_pct,
                        &self.slice_memory_max,
                    )),
                    None => Err("systemd-run not found on PATH".to_string()),
                }
            };
            let (placement, note) = decide_placement(self.enabled, provision);
            if let Some(note) = note {
                eprintln!("[gantry] note: {note}");
            }
            Decision {
                placement,
                systemd_run: if placement == Placement::Slice {
                    systemd_run
                } else {
                    None
                },
            }
        })
    }

    /// Spawn `real` with `args`, slice-placed and per-run-capped when the
    /// slice is active, plainly otherwise. The returned status is the
    /// child's either way (module docs: `--scope` propagates it), so the
    /// caller's verdict mapping is untouched by this wrapper.
    pub fn spawn(&self, real: &Path, args: &[String]) -> std::io::Result<ExitStatus> {
        let decision = self.decide();
        match (&decision.placement, &decision.systemd_run) {
            (Placement::Slice, Some(systemd_run)) => Ok(slice_launch_command(
                systemd_run,
                real,
                args,
                self.cpu_quota_pct,
                &self.memory_max,
            )
            .status()?),
            _ => Command::new(real).args(args).status(),
        }
    }
}

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

    /// Acquire a fallback slot, queueing loudly for at most `max_wait`.
    ///
    /// The queue class comes from the `GANTRY_AGENT` environment convention
    /// ([`QueueClass::from_env`]); tests and future call sites pass one
    /// explicitly via [`Self::admit_as`].
    pub fn admit(&self) -> Result<Admission, SemaphoreError> {
        self.admit_as(QueueClass::from_env())
    }

    /// [`Self::admit`] as an explicit [`QueueClass`] — the seam that keeps
    /// process-environment mutation out of tests and lets a future submission
    /// gate classify by its own signal.
    ///
    /// The queue position line prints on the first wait and whenever the
    /// position changes (plus at most once per [`QUEUE_LINE_INTERVAL`] while
    /// nothing changes) so an agent transcript sees movement without per-poll
    /// spam. An agent's line carries the triage tag ([`queue_line_for`]).
    pub fn admit_as(&self, class: QueueClass) -> Result<Admission, SemaphoreError> {
        let deadline = Instant::now() + self.max_wait;
        let ticket = self.mint_ticket(class)?;
        let mut announced: Option<(u64, Instant)> = None;
        let mut poll = POLL_MIN;

        loop {
            let ahead = self.ahead_count(ticket.number, class)?;

            // Only a waiter with nothing outranking it may try slots — strict
            // order within a class, humans ahead of agents — and (module
            // docs) exactly one contender exists at any instant, so the
            // non-blocking slot flock below has no competitor to lose to.
            if ahead == 0 {
                if let Some(slot) = self.try_slots()? {
                    // The slot flock is now this run's liveness proof; the
                    // ticket's ordering job is done. Release it at once —
                    // kept, it would sit "ahead" of every waiter and hold the
                    // whole queue behind a run that already has its slot,
                    // collapsing the pool to a single concurrent run.
                    drop(ticket);
                    return Ok(Admission::Acquired(SlotGuard { _slot: slot }));
                }
            }

            Self::announce_queue_position(ahead, class, &mut announced);

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
    fn announce_queue_position(
        ahead: u64,
        class: QueueClass,
        announced: &mut Option<(u64, Instant)>,
    ) {
        let now = Instant::now();
        let due = match *announced {
            None => true,
            Some((last_ahead, at)) => {
                last_ahead != ahead || now.duration_since(at) >= QUEUE_LINE_INTERVAL
            }
        };
        if due {
            eprintln!("{}", queue_line_for(ahead, class));
            *announced = Some((ahead, now));
        }
    }

    /// Mint the next FIFO ticket: bump the counter under the queue lock,
    /// create the ticket file create-new (collision-proof even where the lock
    /// degraded), hold its exclusive lock from this instant, and mark it with
    /// the waiter's [`QueueClass`] — the only record of the class another
    /// waiter (which cannot see this process's environment) can read.
    fn mint_ticket(&self, class: QueueClass) -> Result<Ticket, SemaphoreError> {
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

        // Mark the ticket with its queue class before any scanner can count
        // it: a ticket without a readable marker reads as interactive (the
        // conservative default), which would wrongly let an agent waiter
        // count this agent as a human — harmless — but would also make an
        // interactive waiter count an agent ahead of it — not harmless. The
        // write happens under the held lock, and the only reader is a later
        // scan; a scan racing the write reads the conservative default for
        // one poll and self-corrects on the next.
        let mut marked = file;
        marked
            .write_all(class.marker().as_bytes())
            .map_err(|e| SemaphoreError::Io {
                path: path.clone(),
                source: e,
            })?;

        Ok(Ticket {
            number,
            _lock: marked,
        })
    }

    /// Count the live tickets that outrank this waiter under the humans-first
    /// ordering ([`QueueClass`]): a human counts live humans with a smaller
    /// number (strict FIFO among interactive waiters — agents never block a
    /// human); an agent counts every live human ticket (any number — humans
    /// first is not positional) plus live agents with a smaller number (FIFO
    /// among agents). Dead tickets found along the way (lock acquirable =
    /// holder gone) are unlinked; numbers are never reused, so this cannot
    /// race a live holder.
    fn ahead_count(&self, number: u64, class: QueueClass) -> Result<u64, SemaphoreError> {
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
                continue;
            }
            // A live ticket: classify it from its minted marker. Unreadable
            // content reads as interactive — the conservative reading, which
            // for an interactive waiter keeps the position honest and for an
            // agent only ever makes it wait longer, never shorter.
            let theirs = QueueClass::from_marker(&fs::read_to_string(&path).unwrap_or_default());
            let outranks = match (class, theirs) {
                // Agents are outranked by any live human waiter, wherever it
                // sits in the numbering, and by earlier agents.
                (QueueClass::Agent, QueueClass::Interactive) => true,
                (QueueClass::Agent, QueueClass::Agent) => n < number,
                // Humans are outranked only by earlier humans.
                (QueueClass::Interactive, QueueClass::Interactive) => n < number,
                (QueueClass::Interactive, QueueClass::Agent) => false,
            };
            if outranks {
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
/// `[gantry] waiting for local slot (N ahead)`). `ahead` counts the live
/// waiting tickets that outrank this run's (see [`FallbackSemaphore::ahead_count`]).
/// The interactive line is the plan's verbatim; an agent's line carries the
/// triage tag — a human reading the transcript can see at a glance that this
/// waiter is a machine parked behind humans by design, not a hung run.
fn queue_line_for(ahead: u64, class: QueueClass) -> String {
    match class {
        QueueClass::Interactive => format!("[gantry] waiting for local slot ({ahead} ahead)"),
        QueueClass::Agent => format!(
            "[gantry] waiting for local slot ({ahead} ahead) \
             [agent: queued behind humans]"
        ),
    }
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

    // Slice-placed and per-run-capped (plan Component 6): the capped fallback
    // is exactly the run class the boxwide slice exists to bound.
    let status = SlicePlacement::from_config(config).spawn(&real, args);

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
        assert_eq!(
            queue_line_for(0, QueueClass::Interactive),
            "[gantry] waiting for local slot (0 ahead)"
        );
        assert_eq!(
            queue_line_for(17, QueueClass::Interactive),
            "[gantry] waiting for local slot (17 ahead)"
        );
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
        let next = sem.mint_ticket(QueueClass::Interactive).unwrap();
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

        let earlier = sem.mint_ticket(QueueClass::Interactive).unwrap();
        let later = sem.mint_ticket(QueueClass::Interactive).unwrap();

        assert_eq!(
            sem.ahead_count(later.number, QueueClass::Interactive)
                .unwrap(),
            1,
            "the live earlier waiter is ahead"
        );
        assert_eq!(
            sem.ahead_count(earlier.number, QueueClass::Interactive)
                .unwrap(),
            0,
            "nobody is ahead of the head waiter"
        );

        // The earlier waiter gives up (bounded wait, crash — same kernel
        // effect): the next scan must see the queue clear, not wedge on the
        // corpse.
        drop(earlier);
        assert_eq!(
            sem.ahead_count(later.number, QueueClass::Interactive)
                .unwrap(),
            0,
            "a released ticket must stop counting as ahead"
        );
        drop(later);
    }

    // ========================================================================
    // GANTRY_AGENT humans-first ordering (plan Component 6, bf-xj0)
    // ========================================================================

    #[test]
    fn agent_env_convention_truthy_spellings_only() {
        // The conventional truthy spellings mark an agent — case-insensitive,
        // trimmed.
        for v in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            assert_eq!(
                QueueClass::from_env_value(Some(v)),
                QueueClass::Agent,
                "'{v}' must classify as an agent"
            );
        }
        // Unset, empty, or a stray value is interactive: a typo'd value must
        // never demote a human behind machines.
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("agent"),
            Some("2"),
        ] {
            assert_eq!(
                QueueClass::from_env_value(v),
                QueueClass::Interactive,
                "{v:?} must classify as interactive"
            );
        }
    }

    #[test]
    fn ticket_marker_round_trips_and_defaults_interactive() {
        assert_eq!(
            QueueClass::from_marker(QueueClass::Agent.marker()),
            QueueClass::Agent
        );
        assert_eq!(
            QueueClass::from_marker(QueueClass::Interactive.marker()),
            QueueClass::Interactive
        );
        // Unreadable or unrecognized content (a pre-priority ticket, a
        // truncated write) reads as the conservative interactive default.
        for text in ["", "garbage", " agent  ", "INTERACTIVE"] {
            assert_eq!(
                QueueClass::from_marker(text),
                QueueClass::Interactive,
                "'{text}' must read as interactive"
            );
        }
    }

    #[test]
    fn agent_queue_line_carries_the_triage_tag() {
        assert_eq!(
            queue_line_for(2, QueueClass::Interactive),
            "[gantry] waiting for local slot (2 ahead)"
        );
        let line = queue_line_for(2, QueueClass::Agent);
        assert!(
            line.starts_with("[gantry] waiting for local slot (2 ahead)"),
            "the plan's verbatim line prefix must survive: {line}"
        );
        assert!(
            line.contains("agent"),
            "an agent's queue line must carry the triage tag: {line}"
        );
    }

    /// The humans-first ordering at the `ahead_count` level: a human is never
    /// outranked by an agent (wherever it sits in the numbering), an agent is
    /// outranked by *any* live human waiter, and each class is FIFO among
    /// itself.
    #[test]
    #[cfg(unix)]
    fn humans_first_ordering_counts() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 1, 5_000);

        let agent_early = sem.mint_ticket(QueueClass::Agent).unwrap();
        let human_late = sem.mint_ticket(QueueClass::Interactive).unwrap();
        let agent_late = sem.mint_ticket(QueueClass::Agent).unwrap();

        // The late human outranks nothing (agents never block a human).
        assert_eq!(
            sem.ahead_count(human_late.number, QueueClass::Interactive)
                .unwrap(),
            0,
            "an agent waiter must never block a human"
        );
        // Every live human outranks an agent, positionally regardless; agents
        // are FIFO among themselves.
        assert_eq!(
            sem.ahead_count(agent_early.number, QueueClass::Agent)
                .unwrap(),
            1,
            "the early agent waits behind the human"
        );
        assert_eq!(
            sem.ahead_count(agent_late.number, QueueClass::Agent)
                .unwrap(),
            2,
            "the late agent waits behind the human and the early agent"
        );

        // The human leaves: the early agent becomes head of the queue.
        drop(human_late);
        assert_eq!(
            sem.ahead_count(agent_early.number, QueueClass::Agent)
                .unwrap(),
            0,
            "the human leaving must unblock the agent"
        );
        assert_eq!(
            sem.ahead_count(agent_late.number, QueueClass::Agent)
                .unwrap(),
            1,
            "the early agent still outranks the late one"
        );

        drop(agent_early);
        drop(agent_late);
    }

    /// The acceptance scenario, deterministically: an interactive waiter is
    /// already in the queue when a `GANTRY_AGENT=1` run arrives — the agent
    /// queues behind it, and only the interactive waiter is positioned to
    /// take the next free slot.
    #[test]
    #[cfg(unix)]
    fn agent_queues_behind_an_interactive_waiter() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 1, 5_000);

        let human = sem.mint_ticket(QueueClass::Interactive).unwrap();
        let agent = sem.mint_ticket(QueueClass::Agent).unwrap();

        assert_eq!(
            sem.ahead_count(agent.number, QueueClass::Agent).unwrap(),
            1,
            "the agent must queue behind the interactive waiter"
        );
        assert_eq!(
            sem.ahead_count(human.number, QueueClass::Interactive)
                .unwrap(),
            0,
            "the interactive waiter must be the one ahead"
        );

        // Once the human's ticket is gone, the agent is next — the ordering
        // is humans-first, not humans-only.
        drop(human);
        assert_eq!(sem.ahead_count(agent.number, QueueClass::Agent).unwrap(), 0);
        drop(agent);
    }

    /// An agent on an empty pool admits immediately: the ordering is
    /// humans-*first*, not humans-*only* — a `GANTRY_AGENT=1` run must never
    /// stall just because no human is waiting.
    #[test]
    #[cfg(unix)]
    fn agent_admits_immediately_on_an_empty_pool() {
        let dir = TempDir::new().unwrap();
        let sem = semaphore(dir.path().to_path_buf(), 2, 1_000);
        assert!(matches!(
            sem.admit_as(QueueClass::Agent).unwrap(),
            Admission::Acquired(_)
        ));
    }

    // ========================================================================
    // Boxwide gantry.slice sum cap (plan Component 6, bf-xj0)
    // ========================================================================

    /// A `SlicePlacement` with its one-per-process decision pre-seeded, so a
    /// test exercises a chosen placement without probing the real PATH or
    /// touching the real user manager.
    fn placement_with(placement: Placement, systemd_run: Option<PathBuf>) -> SlicePlacement {
        let sp = SlicePlacement {
            enabled: true,
            cpu_quota_pct: 200,
            memory_max: "6G".to_string(),
            slice_cpu_quota_pct: super::DEFAULT_SLICE_CPU_QUOTA_PCT,
            slice_memory_max: super::DEFAULT_SLICE_MEMORY_MAX.to_string(),
            decision: std::sync::OnceLock::new(),
        };
        sp.decision
            .set(Decision {
                placement,
                systemd_run,
            })
            .unwrap();
        sp
    }

    #[test]
    fn slice_unit_content_pins_the_sum_cap() {
        let unit = slice_unit_content(1200, "32G");
        assert!(
            unit.starts_with("# Managed by gantry"),
            "marked as ours: {unit}"
        );
        assert!(unit.contains("[Slice]"));
        // Caps without accounting are merely observed, not enforced.
        assert!(unit.contains("CPUAccounting=true"));
        assert!(unit.contains("MemoryAccounting=true"));
        assert!(unit.contains("CPUQuota=1200%"));
        assert!(unit.contains("MemoryMax=32G"));
        // The configured values travel verbatim.
        assert_eq!(
            slice_unit_content(800, "24G"),
            slice_unit_content(800, "24G")
        );
        assert!(slice_unit_content(800, "24G").contains("CPUQuota=800%"));
        assert!(slice_unit_content(800, "24G").contains("MemoryMax=24G"));
    }

    #[test]
    fn slice_launch_command_matches_the_plan_line() {
        let cmd = slice_launch_command(
            Path::new("/usr/bin/systemd-run"),
            Path::new("/home/u/.cargo/bin/cargo"),
            &["test".to_string(), "--lib".to_string()],
            200,
            "6G",
        );
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "--scope",
                "--user",
                "--slice=gantry.slice",
                "-p",
                "CPUQuota=200%",
                "-p",
                "MemoryMax=6G",
                "-p",
                "MemorySwapMax=0",
                "-q",
                "--",
                "/home/u/.cargo/bin/cargo",
                "test",
                "--lib",
            ],
            "the plan §6 capping line plus --slice, in order: {argv:?}"
        );
        assert_eq!(
            cmd.get_program(),
            std::ffi::OsStr::new("/usr/bin/systemd-run")
        );
    }

    #[test]
    fn placement_decision_branches() {
        // Disabled is a deliberate operator choice: plain, silently.
        assert_eq!(
            decide_placement(false, Err("no systemd".to_string())),
            (Placement::Plain, None)
        );
        // Enabled and provisioned: slice, no note.
        assert_eq!(decide_placement(true, Ok(())), (Placement::Slice, None));
        // Enabled but unavailable: plain, loudly.
        let (placement, note) = decide_placement(true, Err("no user manager".to_string()));
        assert_eq!(placement, Placement::Plain);
        let note = note.expect("a degrade reason must be printed");
        assert!(note.contains("gantry.slice unavailable"));
        assert!(note.contains("no user manager"));
        assert!(
            note.contains("without cgroup caps"),
            "the note must say what was lost: {note}"
        );
    }

    #[test]
    fn unit_file_is_written_only_when_content_differs() {
        let dir = TempDir::new().unwrap();
        let content = slice_unit_content(1200, "32G");

        // Missing: written.
        assert!(write_unit_if_changed(dir.path(), &content).unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(SLICE_NAME)).unwrap(),
            content
        );

        // Same content: not rewritten (a warm box pays no daemon-reload).
        assert!(!write_unit_if_changed(dir.path(), &content).unwrap());

        // A changed cap: rewritten.
        let changed = slice_unit_content(800, "24G");
        assert!(write_unit_if_changed(dir.path(), &changed).unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(SLICE_NAME)).unwrap(),
            changed
        );
    }

    /// The slice-placed spawn end to end, against a fake `systemd-run` that
    /// logs the launch line it was handed and then execs the real command:
    /// the wrapper receives the full plan line (slice flag included), and the
    /// child's exit code propagates through `--scope` unchanged (INV-3).
    #[test]
    #[cfg(unix)]
    fn slice_spawn_launches_through_the_wrapper_and_propagates_the_exit_code() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("argv.log");
        let fake = tmp.path().join("systemd-run");
        // Log every wrapper flag up to `--`, then exec the payload so the
        // status gantry sees is the payload's.
        fs::write(
            &fake,
            // The log path is baked into the script rather than passed as
            // $GANTRY_FAKE_ARGV_LOG: the child inherits this process's
            // environment, and parallel tests setting the same var would
            // race each other's children.
            format!(
                "#!/usr/bin/env bash\n\
                 while [ \"$1\" != \"--\" ]; do \n\
                 printf '%s\\n' \"$1\" >> '{}'; shift; done\n\
                 shift\nexec \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let sp = placement_with(Placement::Slice, Some(fake));
        let status = sp
            .spawn(Path::new("sh"), &["-c".to_string(), "exit 7".to_string()])
            .unwrap();
        assert_eq!(
            status_to_i32(status),
            7,
            "the child's exit code must survive"
        );

        let argv = fs::read_to_string(&log).unwrap();
        for expected in [
            "--scope",
            "--user",
            "--slice=gantry.slice",
            "CPUQuota=200%",
            "MemoryMax=6G",
            "MemorySwapMax=0",
        ] {
            assert!(
                argv.contains(expected),
                "launch line lacks {expected}: {argv}"
            );
        }
    }

    /// The clean degrade: a plain spawn when the slice is unavailable — same
    /// child, same exit code, no wrapper in between.
    #[test]
    #[cfg(unix)]
    fn plain_spawn_is_uncapped_and_faithful() {
        let sp = placement_with(Placement::Plain, None);
        let status = sp
            .spawn(Path::new("sh"), &["-c".to_string(), "exit 3".to_string()])
            .unwrap();
        assert_eq!(status_to_i32(status), 3);
    }

    /// N concurrent slice-placed spawns all launch through the wrapper: the
    /// slice (not gantry) enforces the sum cap, so the invariant gantry must
    /// uphold is that *every* concurrent capped run lands inside
    /// `--slice=gantry.slice` — none escapes as a plain spawn.
    #[test]
    #[cfg(unix)]
    fn concurrent_slice_spawns_all_carry_the_slice() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("argv.log");
        let fake = tmp.path().join("systemd-run");
        fs::write(
            &fake,
            // The log path is baked into the script rather than passed as
            // $GANTRY_FAKE_ARGV_LOG: the child inherits this process's
            // environment, and parallel tests setting the same var would
            // race each other's children.
            format!(
                "#!/usr/bin/env bash\n\
                 while [ \"$1\" != \"--\" ]; do \n\
                 printf '%s\\n' \"$1\" >> '{}'; shift; done\n\
                 shift\nexec \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let sp = std::sync::Arc::new(placement_with(Placement::Slice, Some(fake)));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let sp = std::sync::Arc::clone(&sp);
                std::thread::spawn(move || sp.spawn(Path::new("true"), &[]).unwrap().success())
            })
            .collect();
        for w in workers {
            assert!(w.join().unwrap(), "every concurrent run must succeed");
        }

        let launches = fs::read_to_string(&log).unwrap();
        assert_eq!(
            launches.matches("--slice=gantry.slice").count(),
            4,
            "all 4 concurrent runs must land inside the slice: {launches}"
        );
    }
}
