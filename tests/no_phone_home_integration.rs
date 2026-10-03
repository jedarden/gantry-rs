//! Positive control for the no-phone-home pledge (plan S-7, INV-6): the
//! LD_PRELOAD net observer (`tests/no_phone_home/netobserver.c`) is built
//! at test time from the committed sources and proven to record a real
//! `connect()` from a child process — and proven inert when it is not
//! preloaded.
//!
//! The later slices of this suite build on the control: a full intercepted
//! gantry run asserted against the empty connection allow-list would pass
//! vacuously if the preload were silently broken (wrong symbol, dlsym
//! failure, lost env var), so the control closes that hole first.
//!
//! The observer is honest about its reach: static binaries, raw syscalls,
//! and glibc's internal resolver traffic can bypass LD_PRELOAD
//! interposition. What it enforces is the realistic regression surface —
//! no code linked into the run may reach the network through the normal
//! libc socket API, which is exactly how a telemetry/update-check
//! dependency would.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::TempDir;

/// Path to the preload observer source.
fn observer_src() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/no_phone_home/netobserver.c");
    path
}

/// Path to the positive-control probe source.
fn probe_src() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/no_phone_home/connect_probe.c");
    path
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

/// The observer shared library and the positive-control probe, compiled
/// once per test process into a throwaway dir.
struct ObserverTools {
    /// The LD_PRELOAD library.
    observer_so: PathBuf,
    /// The probe binary that attempts one TCP connect.
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

/// Read the netlog back: one line per attempted IP destination. A missing
/// file means no attempt was ever made — the expected green shape.
fn read_attempts(netlog: &Path) -> Vec<String> {
    match fs::read_to_string(netlog) {
        Ok(content) => content.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Positive control: the observer records what it sees and stays inert
/// when absent. Without this, a silently broken preload (wrong symbol,
/// dlsym failure, lost env var) would let the later empty allow-list
/// assertion pass vacuously — the test would prove nothing.
#[test]
fn the_observer_records_attempts_and_is_inert_without_the_preload() {
    let tools = tools();

    // Preloaded probe → exactly one record naming the attempted destination.
    // The dir binding must outlive the probe: a dropped TempDir deletes the
    // directory out from under the observer's append.
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
