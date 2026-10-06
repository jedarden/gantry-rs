//! Integration coverage for the no-phone-home pledge on the onboarding
//! path (plan S-7 / INV-6, the Phase 3 gate "the no-phone-home test is
//! green in CI"): `gantry init` and the whole `gantry init --ssh` ladder —
//! reach, git, cargo, install, preset write, and the finishing `doctor
//! --e2e` canary — must attempt **no outbound IP connection of any kind**.
//!
//! The pledge is CI-enforced, not prose, so the suite drives the real
//! binary through the real onboarding flow against the loopback world the
//! `init --ssh` integration tests use (a stand-in `ssh` that executes the
//! "remote" commands in a local sh, a fixture HOME whose toolchain is the
//! rustup shape, and the canary's own file:// bare remote): a world with
//! no legitimate IP destination. Into that world it injects the same
//! LD_PRELOAD observer the remote-run suite uses
//! (`tests/no_phone_home/netobserver.c`, built at test time) — one netlog
//! line for every AF_INET/AF_INET6 `connect`/`sendto`/`sendmsg` any
//! process in the onboarding tree attempts: gantry itself, the stand-in
//! ssh, the probes it runs, the executor, the canary's cargo. The
//! assertion is the empty allow-list: with a loopback ssh stand-in and a
//! file:// canary remote there is nothing onboarding may legitimately
//! connect to, so any record is a phone-home — a gantry-operated telemetry
//! endpoint, a third-party update check, a resolver dial — whatever its
//! destination.
//!
//! Two guards keep the assertion honest. The positive control proves this
//! suite's own compiled observer actually records (both probe modes: the
//! TCP shape the positive controls dial and the UDP shape the mutation run
//! dials with) and stays inert when not preloaded. The mutation run proves
//! the assertion trips on a real phone-home made where it matters: the
//! stand-in ssh dials a non-loopback RFC 5737 TEST-NET address from inside
//! the onboarding tree before every leg — instantly and with no packet on
//! the wire, the way a UDP telemetry beacon would — and onboarding still
//! finishes green, which is exactly why the pledge needs an assertion of
//! its own: the run cannot notice, only the netlog can.
//!
//! The observer is honest about its reach, and so is this suite: static
//! binaries, raw syscalls, and glibc's internal resolver traffic can
//! bypass LD_PRELOAD interposition. What it enforces is the realistic
//! regression surface — no code linked into onboarding may reach the
//! network through the normal libc socket API, which is exactly how a
//! telemetry/update-check dependency would.

use std::env;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// Serializes the suite's observed runs: each test drives the real onboarding
/// flow under the LD_PRELOAD observer — its own world and netlog, but an
/// interleaving the runs do not survive. The remote-run suite fixed this
/// failure class the same way (its runs lost records and picked up foreign
/// ones under cargo's default parallel test threads), so one observed run at
/// a time is the contract here too, before the symptom can be relearned.
static OBSERVED_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Take [`OBSERVED_RUN_LOCK`] for one test's duration, poison-recovered the
/// way the remote-run suite takes its lock: a panicking prior test must not
/// wedge the suite for every test after it.
fn observed_run() -> MutexGuard<'static, ()> {
    OBSERVED_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Path to the preload observer source — the very copy the remote-run
/// no-phone-home suite compiles, so both suites enforce one observer.
fn observer_src() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/no_phone_home/netobserver.c");
    path
}

/// Path to the positive-control/mutation probe source.
fn probe_src() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/no_phone_home/connect_probe.c");
    path
}

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// Resolve `name` on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Locate a C compiler: `$CC`, then the usual names on PATH. Any
/// environment that builds this crate has one — cc is rustc's linker.
fn find_cc() -> Option<PathBuf> {
    if let Ok(cc) = env::var("CC") {
        let direct = PathBuf::from(&cc);
        if direct.is_file() {
            return Some(direct);
        }
        if let Some(found) = which(&cc) {
            return Some(found);
        }
    }
    ["cc", "gcc", "clang"].iter().find_map(|name| which(name))
}

/// The observer shared library and the probe, compiled once per test
/// process into a throwaway dir.
struct ObserverTools {
    /// The LD_PRELOAD library.
    observer_so: PathBuf,
    /// The probe binary that attempts one connect.
    probe_bin: PathBuf,
    /// Backing dir — kept alive (and best-effort removed) with the struct.
    _dir: TempDir,
}

fn compile_observer_tools() -> Result<ObserverTools, String> {
    let cc = find_cc().ok_or_else(|| {
        "no C compiler for the net observer (looked at $CC, cc, gcc, clang)".to_string()
    })?;
    let dir = TempDir::new().map_err(|e| format!("cannot create observer build dir: {e}"))?;

    let observer_so = dir.path().join("libgantry_netobserver.so");
    run_cc(
        &cc,
        &[
            "-O1",
            "-fPIC",
            "-shared",
            "-o",
            observer_so.to_str().expect("temp path is valid utf-8"),
            observer_src()
                .to_str()
                .expect("manifest path is valid utf-8"),
            "-ldl",
        ],
    )?;

    let probe_bin = dir.path().join("connect-probe");
    run_cc(
        &cc,
        &[
            "-O2",
            "-o",
            probe_bin.to_str().expect("temp path is valid utf-8"),
            probe_src().to_str().expect("manifest path is valid utf-8"),
        ],
    )?;

    Ok(ObserverTools {
        observer_so,
        probe_bin,
        _dir: dir,
    })
}

fn run_cc(cc: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new(cc)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cc.display()))?;
    if !output.status.success() {
        return Err(format!(
            "cc {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Compile the observer and probe once for the whole test process.
fn tools() -> &'static ObserverTools {
    static TOOLS: OnceLock<ObserverTools> = OnceLock::new();
    TOOLS.get_or_init(|| {
        compile_observer_tools().expect("the net observer must compile for this suite to run")
    })
}

/// The mutation run's phone-home destination: 203.0.113.1 is RFC 5737
/// TEST-NET-3 — reserved for documentation, announced by no one, routed by
/// nothing. The probe's UDP connect only pins the destination (no packet,
/// no wait), so the dial is instant and inert on every box while still
/// being exactly the shape the observer exists to catch: a non-loopback
/// destination handed to connect(2) from inside the onboarding tree. The
/// TCP equivalent is unusable here — a blackholed SYN stalls the leg for
/// the full retransmit window.
const MUTATION_DST_IP: &str = "203.0.113.1";
const MUTATION_DST_PORT: &str = "9";

/// Read the netlog back: one line per attempted IP destination. A missing
/// file means no attempt was ever made — the expected green shape.
fn read_attempts(netlog: &Path) -> Vec<String> {
    match fs::read_to_string(netlog) {
        Ok(content) => content.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// The INV-6 assertion, factored out so the mutation run can prove it
/// trips: the recorded netlog must be the empty allow-list. Any record is
/// a phone-home whatever its destination, and the panic names the syscall
/// and peer of every offending record.
fn assert_empty_allow_list(netlog: &Path) {
    let attempts = read_attempts(netlog);
    assert!(
        attempts.is_empty(),
        "phone-home detected: the onboarding path attempted outbound IP \
         connections, expected none (the loopback ssh stand-in and the \
         file:// canary remote need no network):\n{}",
        attempts.join("\n")
    );
}

/// Positive control: this suite's observer records what it sees, in both
/// probe modes, and stays inert when absent. Without this, a silently
/// broken preload (wrong symbol, dlsym failure, lost env var) would let
/// the later empty allow-list assertions pass vacuously — the tests would
/// prove nothing. The UDP control is not redundancy: the mutation run's
/// phone-home rides that mode, so its ability to record a non-loopback
/// destination is proven here, beside the ladder, before it is relied on.
#[test]
fn the_observer_records_attempts_and_is_inert_without_the_preload() {
    let tools = tools();

    // Preloaded TCP probe at loopback → exactly one record naming the
    // attempted destination. The dir binding must outlive the probe: a
    // dropped TempDir deletes the directory out from under the observer's
    // append.
    let log_dir = TempDir::new().expect("create netlog dir");
    let netlog = log_dir.path().join("netlog");
    Command::new(&tools.probe_bin)
        .args(["127.0.0.1", "1"])
        .env("LD_PRELOAD", tools.observer_so.display().to_string())
        .env("GANTRY_TEST_NETLOG", &netlog)
        .output()
        .expect("spawn the connect probe");
    let attempts = read_attempts(&netlog);
    assert_eq!(
        attempts.len(),
        1,
        "one probe connect must leave exactly one record, got: {attempts:?}"
    );
    assert!(
        attempts[0].contains("connect") && attempts[0].contains("127.0.0.1:1"),
        "the record must name the syscall and destination, got: {}",
        attempts[0]
    );

    // Preloaded UDP probe at the TEST-NET mutation destination → exactly
    // one record naming that non-loopback peer: the dial the mutation run
    // injects is instant (nothing sent, nothing waited) and recorded.
    let log_dir = TempDir::new().expect("create netlog dir");
    let netlog = log_dir.path().join("netlog");
    Command::new(&tools.probe_bin)
        .args([MUTATION_DST_IP, MUTATION_DST_PORT, "udp"])
        .env("LD_PRELOAD", tools.observer_so.display().to_string())
        .env("GANTRY_TEST_NETLOG", &netlog)
        .output()
        .expect("spawn the udp connect probe");
    let attempts = read_attempts(&netlog);
    assert_eq!(
        attempts.len(),
        1,
        "one udp probe connect must leave exactly one record, got: {attempts:?}"
    );
    assert!(
        attempts[0].contains("connect")
            && attempts[0].contains(&format!("dst={MUTATION_DST_IP}:{MUTATION_DST_PORT}")),
        "the record must name the syscall and the non-loopback destination, got: {}",
        attempts[0]
    );

    // Netlog env without the preload → the observer is not in the process,
    // so nothing may record: the env var alone never creates evidence.
    let log_dir = TempDir::new().expect("create netlog dir");
    let netlog = log_dir.path().join("netlog");
    Command::new(&tools.probe_bin)
        .args(["127.0.0.1", "1"])
        .env("GANTRY_TEST_NETLOG", &netlog)
        .output()
        .expect("spawn the unpreloaded connect probe");
    assert!(
        !netlog.exists(),
        "without LD_PRELOAD the observer must record nothing, got: {:?}",
        read_attempts(&netlog)
    );
}

/// The loopback world one observed onboarding happens in: the stand-in
/// `ssh` shape the `init --ssh` integration tests prove out, with the
/// simulated host's git on a deliberately stripped PATH and cargo only at
/// the rustup location, so the ladder the observer watches is the real
/// one — rustup fallback probe, executor install, preset write, canary.
struct Loopback {
    /// World root: the stand-in ssh lives here too.
    dir: TempDir,
    /// The simulated host's home — and the spawned init's HOME, so the
    /// preset, its backup, and the state dir all land inside the fixture.
    home: PathBuf,
    /// The dir holding the stand-in `ssh`, prepended to the spawned PATH.
    ssh_bin: PathBuf,
}

/// The loopback ssh stand-in. argv is `<destination> <remote command…>`;
/// the destination names the loopback and is ignored. Env knobs:
/// `LOOPBACK_SSH_HOME`/`LOOPBACK_SSH_PATH` pin the "remote" side's HOME
/// and PATH (the fixture tree), and `GANTRY_TEST_DIAL_PROBE`, when set, is
/// a probe binary every leg dials before doing its work — the mutation
/// run's phone-home hook, a non-loopback connect made inside the
/// onboarding tree where the run cannot notice it.
const LOOPBACK_SSH_SH: &str = r#"#!/usr/bin/env sh
# Loopback ssh fixture for the onboarding no-phone-home integration tests.
set -u
# The simulated host's PATH (pinned below) deliberately strips the system
# dirs — that is the whole rustup-shape point — so the fixture must note
# where sh lives before the pin: it is the shell the remote commands are
# handed to at the bottom, the way sshd hands them to the user's shell.
SH=$(command -v sh)
if [ -n "${LOOPBACK_SSH_HOME:-}" ]; then
    HOME=$LOOPBACK_SSH_HOME
    export HOME
fi
if [ -n "${LOOPBACK_SSH_PATH:-}" ]; then
    PATH=$LOOPBACK_SSH_PATH
    export PATH
fi
shift
cmd=$1
# The mutation run's phone-home hook: one non-loopback dial per leg,
# before the leg's real work. The probe exits 0 whatever the connect did,
# so the ladder above is unchanged — only the netlog can tell.
if [ -n "${GANTRY_TEST_DIAL_PROBE:-}" ]; then
    "$GANTRY_TEST_DIAL_PROBE" 203.0.113.1 9 udp
fi
exec "$SH" -c "$cmd"
"#;

impl Loopback {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");

        // The stand-in ssh lives here, first on the spawned process's PATH.
        let ssh_bin = dir.path().join("ssh-bin");
        fs::create_dir_all(&ssh_bin).unwrap();
        let ssh = ssh_bin.join("ssh");
        fs::write(&ssh, LOOPBACK_SSH_SH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();

        // The simulated host's PATH dir: git (the executor fetches with it)
        // plus the ordinary coreutils the install leg's remote commands
        // need. Cargo stays off this PATH — the rustup shape below is the
        // point, and it is what the probe's fallback exists for.
        let host_bin = home.join("bin");
        fs::create_dir_all(&host_bin).unwrap();
        for tool in ["git", "mkdir", "cat", "chmod"] {
            symlink_tool(tool, &host_bin.join(tool), &home);
        }

        // A pre-existing user config the preset must back up, not destroy —
        // the re-onboarding shape. Nothing here asserts on the backup (the
        // init --ssh suite owns that); the file exists so the observed
        // ladder is the real one, backup leg included.
        fs::create_dir_all(home.join(".config/gantry")).unwrap();
        fs::write(
            home.join(".config/gantry/config.toml"),
            "[local]\ncpu_quota_pct = 150\n",
        )
        .unwrap();

        Loopback { dir, home, ssh_bin }
    }

    /// Spawn the real `gantry` binary with `args` against the loopback,
    /// HOME at the fixture, the observer preloaded into the whole tree, and
    /// every outbound IP destination recorded in `netlog`.
    fn run_observed(&self, netlog: &Path, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.ssh_bin.display(),
            env::var("PATH").unwrap_or_default()
        );

        // Prepend alongside any inherited preload rather than clobbering it.
        let preload = match env::var_os("LD_PRELOAD") {
            Some(existing) if !existing.is_empty() => format!(
                "{}:{}",
                existing.to_string_lossy(),
                tools().observer_so.display()
            ),
            _ => tools().observer_so.display().to_string(),
        };

        Command::new(gantry_binary())
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", &path)
            .env("LOOPBACK_SSH_HOME", &self.home)
            .env("LOOPBACK_SSH_PATH", self.home.join("bin"))
            .env("LD_PRELOAD", preload)
            .env("GANTRY_TEST_NETLOG", netlog)
            // XDG overrides would redirect the config/state layers away
            // from the isolated HOME, so they are stripped rather than set.
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .output()
            .expect("spawn the gantry binary")
    }
}

/// Symlink `name` into the fixture tree, resolved from the host. The
/// simulated host's PATH deliberately carries no shells, and `#!`
/// interpreter lookup goes through PATH — so a host wrapper script (a
/// `#!/usr/bin/env bash` cargo interceptor, say) that runs fine from the
/// invoking shell dies there with exit 126, which the probe would then
/// report as a found-but-broken toolchain. Link only a candidate that
/// execs standalone, verified the way the probe runs it: under the
/// fixture's HOME with an empty PATH. Cargo prefers the real toolchain
/// binary — the running `CARGO`, then the installed rustup toolchains —
/// over `command -v`'s answer, and the rustup shim is no candidate at
/// all: with the fixture's HOME it would download a toolchain mid-test
/// instead of running one.
fn symlink_tool(name: &str, dest: &Path, exec_home: &Path) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if name == "cargo" {
        if let Ok(cargo) = env::var("CARGO") {
            candidates.push(PathBuf::from(cargo));
        }
        let mut rustup_homes: Vec<PathBuf> = Vec::new();
        if let Ok(h) = env::var("RUSTUP_HOME") {
            rustup_homes.push(PathBuf::from(h));
        }
        if let Ok(h) = env::var("HOME") {
            rustup_homes.push(PathBuf::from(h).join(".rustup"));
        }
        for rustup_home in rustup_homes {
            let mut bins: Vec<PathBuf> = fs::read_dir(rustup_home.join("toolchains"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path().join("bin/cargo"))
                .collect();
            bins.sort();
            bins.reverse(); // named toolchains (stable, 1.99.0) before stale ones
            candidates.extend(bins);
        }
    }
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .expect("run sh");
    assert!(out.status.success(), "{name} must be on the test PATH");
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !found.is_empty() {
        candidates.push(PathBuf::from(found));
    }
    let source = candidates
        .iter()
        .find(|candidate| execs_standalone(candidate, exec_home))
        .unwrap_or_else(|| {
            panic!(
                "{name}: none of {candidates:?} execs under the fixture's \
                 HOME with an empty PATH — only a standalone executable can \
                 be linked (a `#!` wrapper script whose interpreter is found \
                 through PATH cannot run on the simulated host)"
            )
        });
    symlink(source, dest).unwrap_or_else(|e| panic!("symlink {name} -> {}: {e}", dest.display()));
}

/// Whether `tool` runs `--version` with the fixture's HOME and nothing on
/// PATH — the exec condition the simulated host imposes: the kernel
/// resolves the binary itself, with no interpreter left to look up.
fn execs_standalone(tool: &Path, exec_home: &Path) -> bool {
    Command::new(tool)
        .arg("--version")
        .env("PATH", "")
        .env("HOME", exec_home)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The cargo file the host has at the rustup location — off the remote
/// PATH, exactly where the probe's fallback must find it.
fn install_rustup_style_cargo(home: &Path) {
    let cargo_bin = home.join(".cargo/bin");
    fs::create_dir_all(&cargo_bin).unwrap();
    symlink_tool("cargo", &cargo_bin.join("cargo"), home);
}

/// Bare `gantry init` is a usage error, and even a usage path makes no
/// network promise exceptions for itself: the observer rides the real
/// binary, the parse rejects, and the netlog must come back empty.
#[test]
fn gantry_init_usage_errors_open_no_ip_connection() {
    let _guard = observed_run();
    let world = Loopback::new();
    let netlog = world.dir.path().join("netlog");

    let output = world.run_observed(&netlog, &["init"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `gantry init` must be a usage error; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_empty_allow_list(&netlog);
}

/// The S-7 onboarding assertion: the whole `gantry init --ssh` ladder —
/// reach, git, cargo (the rustup fallback), executor install, preset write
/// with backup, and the finishing `doctor --e2e` canary — attempts no IP
/// connection of any kind. Exit 0 and the green canary prove the ladder
/// actually ran to the end (a half-run would prove little); the empty
/// allow-list then says that green onboarding stayed off the network
/// entirely: no telemetry, no update check, no resolver dial.
#[test]
fn init_ssh_onboarding_opens_no_ip_connection() {
    let _guard = observed_run();
    let world = Loopback::new();
    install_rustup_style_cargo(&world.home);
    let netlog = world.dir.path().join("netlog");

    let output = world.run_observed(&netlog, &["init", "--ssh", "ops@loopback.test"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "onboarding must run the whole ladder green, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("E2E test: round trip passed"),
        "onboarding must end with the green canary — a half-run proves \
         nothing about the legs' network behavior; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ops@loopback.test is ready"),
        "the ready line must name the onboarded host; stdout:\n{stdout}"
    );

    assert_empty_allow_list(&netlog);
}

/// Mutation run: the same observed onboarding, but every leg now dials a
/// non-loopback TEST-NET address from inside the onboarding tree first —
/// a real phone-home made where one would be made (the telemetry call a
/// regression would add sits exactly here). This is the non-vacuous proof
/// in the direction the positive control cannot cover: the control proves
/// the observer records from a bare probe beside the run, this proves a
/// phone-home made anywhere in the onboarding tree lands in the netlog
/// and trips the empty allow-list with 'phone-home detected' naming the
/// offending peer — while the onboarding itself still finishes green,
/// which is why only the netlog can catch it.
#[test]
fn an_onboarding_phone_home_is_caught() {
    let _guard = observed_run();
    let world = Loopback::new();
    install_rustup_style_cargo(&world.home);
    let netlog = world.dir.path().join("netlog");

    // Stand the mutation hook in via the world's own env knob: every leg
    // dials the TEST-NET destination before doing its work.
    let path = format!(
        "{}:{}",
        world.ssh_bin.display(),
        env::var("PATH").unwrap_or_default()
    );
    let preload = tools().observer_so.display().to_string();
    let output = Command::new(gantry_binary())
        .args(["init", "--ssh", "ops@loopback.test"])
        .env("HOME", &world.home)
        .env("PATH", &path)
        .env("LOOPBACK_SSH_HOME", &world.home)
        .env("LOOPBACK_SSH_PATH", world.home.join("bin"))
        .env(
            "GANTRY_TEST_DIAL_PROBE",
            tools().probe_bin.display().to_string(),
        )
        .env("LD_PRELOAD", preload)
        .env("GANTRY_TEST_NETLOG", &netlog)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .expect("spawn the gantry binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // The phone-home is not an error the run can see: the probe exits 0
    // whatever the connect did, so onboarding still runs the whole ladder
    // green — exactly why the pledge needs an assertion of its own.
    assert!(
        output.status.success(),
        "the phone-homing onboarding must still run green (the run cannot \
         notice the dial), got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("E2E test: round trip passed"),
        "the mutation run must reach the green canary too; stdout:\n{stdout}"
    );

    // The assertion must trip on what the run recorded.
    let trip = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_empty_allow_list(&netlog);
    }));
    let payload = match trip {
        Ok(()) => panic!(
            "the legs' real non-loopback dials must trip the empty \
             allow-list — the netlog holds {attempts:?}",
            attempts = read_attempts(&netlog)
        ),
        Err(payload) => payload,
    };
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
        })
        .expect("the assertion panics with a string payload");

    assert!(
        message.contains("phone-home detected"),
        "the failure must be recognisable as a phone-home, got: {message}"
    );
    assert!(
        message.contains(&format!(
            "connect dst={MUTATION_DST_IP}:{MUTATION_DST_PORT}"
        )),
        "the report must name the syscall and the offending peer the legs \
         dialed, got: {message}"
    );
}
