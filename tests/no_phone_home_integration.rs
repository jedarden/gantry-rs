//! Integration coverage for the no-phone-home pledge (plan S-7, INV-6):
//! a full gantry run — config load, GitGate, epoch-ref push, backend
//! dispatch, executor run, verdict wait — must open **no IP network
//! connection at all**.
//!
//! The pledge is CI-enforced, not prose, so the suite drives the real
//! binary through the production remote pipeline against the doctor --e2e
//! canary's loopback world (bare `file://` remote, reference executor
//! behind the command-template backend): a world with no legitimate IP
//! destination. Into that world it injects an LD_PRELOAD observer
//! (`tests/no_phone_home/netobserver.c`, built at test time) that appends
//! one line to a netlog for every AF_INET/AF_INET6 `connect`/`sendto`/
//! `sendmsg` any process in the run's tree attempts — gantry itself, git,
//! the executor shell, the toolchain — without altering the calls. The
//! assertion is the empty allow-list: with a loopback backend and a
//! file:// remote there is nothing the run may legitimately connect to,
//! so any record in the netlog is a phone-home, whatever its destination.
//!
//! Two guards keep the assertion honest. The positive control proves the
//! observer actually records (a silently broken preload — wrong symbol,
//! dlsym failure, lost env var — would otherwise let the main assertion
//! pass vacuously) and stays inert when not preloaded. The mutation run
//! proves the assertion trips on a real phone-home made where it matters:
//! a fixture that dials a loopback listener from inside the run's own
//! process tree, which the run cannot notice and only the netlog can.
//!
//! The observer is honest about its reach: static binaries, raw syscalls,
//! and glibc's internal resolver traffic can bypass LD_PRELOAD
//! interposition. What it enforces is the realistic regression surface —
//! no code linked into the run may reach the network through the normal
//! libc socket API, which is exactly how a telemetry/update-check
//! dependency would.

use std::env;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// Path to the reference executor — the same script the doctor --e2e
/// canary embeds and the shipped command-template preset runs.
fn executor_path() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("contrib/gantry-exec.sh");
    path.to_str()
        .expect("manifest path is valid utf-8")
        .to_string()
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

/// The INV-6 assertion, factored out so the mutation run can prove it
/// trips: the recorded netlog must be the empty allow-list. Any record is
/// a phone-home whatever its destination, and the panic names the syscall
/// and peer of every offending record.
fn assert_empty_allow_list(netlog: &Path) {
    let attempts = read_attempts(netlog);
    assert!(
        attempts.is_empty(),
        "phone-home detected: the run attempted outbound IP connections, \
         expected none (the loopback backend and file:// remote need no \
         network):\n{}",
        attempts.join("\n")
    );
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

/// The loopback world one observed run happens in: the doctor --e2e
/// canary's fixture shape (bare remote + known-good fixture repo), plus
/// the isolated HOME the run's config/state layering lands in and the
/// `cargo` symlink that makes the real binary take the intercepted path.
struct LoopbackWorld {
    /// Isolated HOME: the user config layer, gantry's state dir (kill
    /// switch, LKG snapshot) and the runlog all live here.
    home: TempDir,
    /// World root: bare remote, work repo, cargo symlink, executor state.
    world: TempDir,
    /// The fixture work repo the run is invoked from.
    repo_dir: PathBuf,
    /// Symlink named `cargo` pointing at the gantry binary, outside the
    /// repo so the gate's clean-tree check never sees it.
    cargo_link: PathBuf,
}

/// The canary fixture crate: zero dependencies (the executor's cargo test
/// needs no crates.io) with one known-good test.
const FIXTURE_CARGO_TOML: &str = "\
[package]
name = \"gantry-no-phone-home-fixture\"
version = \"0.1.0\"
edition = \"2021\"
";

const FIXTURE_LIB_RS: &str = "\
//! Fixture for the no-phone-home integration test: a minimal crate with
//! one known-good test, run remotely by the reference executor.

#[cfg(test)]
mod fixture {
    #[test]
    fn passes_without_any_network() {
        assert_eq!(2 + 2, 4);
    }
}
";

/// The mutation fixture: the same zero-dependency crate whose second test
/// opens a real AF_INET connection to a loopback listener it binds — a
/// genuine phone-home made inside the run's process tree, not a probe
/// beside it. The run itself cannot notice (the mutant still passes its
/// suite); only the netlog the observer records can.
const CONNECTING_FIXTURE_LIB_RS: &str = "\
//! Mutation fixture for the no-phone-home integration test: the
//! known-good test plus one real loopback connect.

#[cfg(test)]
mod fixture {
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn passes_without_any_network() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn phones_home_over_loopback() {
        let listener = TcpListener::bind(\"127.0.0.1:0\").expect(\"bind a loopback listener\");
        let peer = listener.local_addr().expect(\"the listener address\");
        let _stream = TcpStream::connect(peer).expect(\"connect to the listener\");
    }
}
";

/// Build the loopback world around the known-good fixture.
fn build_loopback_world() -> LoopbackWorld {
    build_loopback_world_with_fixture(FIXTURE_LIB_RS)
}

/// Build the loopback world with a chosen fixture crate body — the
/// known-good one for the assertion run, the connecting mutant for the
/// mutation run. The fixture repo is committed (the gate
/// demands a clean tree) with its only remote named `origin` — the
/// configured `ci_remote` default — pointing at the bare `file://` repo.
fn build_loopback_world_with_fixture(lib_rs: &str) -> LoopbackWorld {
    let world = TempDir::new().expect("create loopback world dir");
    let home = TempDir::new().expect("create isolated HOME");

    // Bare loopback remote: real git transport, no service.
    let remote_dir = world.path().join("remote.git");
    git_in(
        world.path(),
        &[
            "init",
            "--bare",
            "--quiet",
            remote_dir.to_str().expect("temp path is valid utf-8"),
        ],
    );

    // Work repo with the committed fixture.
    let repo_dir = world.path().join("repo");
    git_in(
        world.path(),
        &[
            "init",
            "--quiet",
            repo_dir.to_str().expect("temp path is valid utf-8"),
        ],
    );
    git_in(&repo_dir, &["config", "user.name", "no-phone-home fixture"]);
    git_in(
        &repo_dir,
        &["config", "user.email", "no-phone-home@localhost"],
    );
    fs::write(repo_dir.join("Cargo.toml"), FIXTURE_CARGO_TOML).expect("write fixture Cargo.toml");
    fs::create_dir_all(repo_dir.join("src")).expect("create fixture src/");
    fs::write(repo_dir.join("src/lib.rs"), lib_rs).expect("write fixture src/lib.rs");
    git_in(&repo_dir, &["add", "."]);
    git_in(
        &repo_dir,
        &["commit", "--quiet", "-m", "no-phone-home fixture"],
    );
    git_in(
        &repo_dir,
        &[
            "remote",
            "add",
            "origin",
            format!("file://{}", remote_dir.display()).as_str(),
        ],
    );

    // The intercepted path: argv[0]="cargo" routes the real binary through
    // the same shim profile production uses. Outside the repo, so the tree
    // the gate inspects stays exactly the committed fixture.
    let cargo_link = world.path().join("cargo");
    symlink(gantry_binary(), &cargo_link).expect("create cargo symlink to the gantry binary");

    // User config layer: explicit command-backend opt-in (Tier-0 is the
    // zero-config default, and the repo layer cannot make this choice —
    // trust boundary S-2). The shipped preset reads GANTRY_EXEC_PATH.
    let config_dir = home.path().join(".config/gantry");
    fs::create_dir_all(&config_dir).expect("create user config dir");
    fs::write(
        config_dir.join("config.toml"),
        "[remote]\nbackend = \"command\"\n",
    )
    .expect("write user config");

    // Private executor state dir — never the shared /tmp/gantry-runs default.
    fs::create_dir_all(world.path().join("executor-state")).expect("create executor state dir");

    LoopbackWorld {
        home,
        world,
        repo_dir,
        cargo_link,
    }
}

/// Run `git <args>` in `dir`; panic with stderr on failure.
fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

/// Run one full intercepted `cargo test` through the real gantry binary in
/// the loopback world, with the observer preloaded into the whole process
/// tree and every outbound IP destination recorded in `netlog`.
fn run_gantry_under_observer(world: &LoopbackWorld, netlog: &Path) -> Output {
    // Mirror tests/integration.rs: the reference executor runs the real
    // toolchain inside the fetched worktree, so it must not inherit
    // whatever `cargo` sits first in this process's PATH (the fleet
    // interceptor on shimmed boxes), and the rustup proxy needs
    // CARGO_HOME/RUSTUP_HOME pinned to resolve toolchains under the
    // redirected HOME. Boxes with a system cargo keep the PATH lookup.
    let orig_home = PathBuf::from(env::var_os("HOME").expect("HOME is set"));
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| orig_home.join(".cargo"));
    let rustup_home = env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| orig_home.join(".rustup"));
    let home_cargo = cargo_home.join("bin").join("cargo");

    // Prepend alongside any inherited preload rather than clobbering it.
    let preload = match env::var_os("LD_PRELOAD") {
        Some(existing) if !existing.is_empty() => {
            format!(
                "{}:{}",
                existing.to_string_lossy(),
                tools().observer_so.display()
            )
        }
        _ => tools().observer_so.display().to_string(),
    };

    Command::new(&world.cargo_link)
        .current_dir(&world.repo_dir)
        .args(["test"])
        .env("HOME", world.home.path())
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .envs(home_cargo.is_file().then(|| {
            (
                "GANTRY_EXEC_CARGO",
                home_cargo.to_string_lossy().into_owned(),
            )
        }))
        .env("GANTRY_EXEC_PATH", executor_path())
        .env(
            "GANTRY_EXEC_STATE_DIR",
            world.world.path().join("executor-state"),
        )
        .env("LD_PRELOAD", preload)
        .env("GANTRY_TEST_NETLOG", netlog)
        // The kill-switch variables are this test's own contract with the
        // binary: neither the interception kill switch nor a `gantry off`
        // inherited from an outer environment may turn the run local.
        .env_remove("GANTRY_LOCAL")
        .env_remove("GANTRY_ON")
        // XDG overrides would redirect the config/state layers away from
        // the isolated HOME, so they are stripped rather than set.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .expect("spawn the gantry binary")
}

/// The S-7 assertion itself: a full remote run — config load, GitGate,
/// epoch-ref push, backend dispatch, executor run, verdict wait — opens no
/// IP connection of any kind. Exit 0 and the `submitted:` line prove the
/// run actually took the remote path (a silent local fallback would also
/// exit 0, but exercises none of the pipeline under test); the empty
/// allow-list then says that green run stayed off the network entirely.
#[test]
fn a_full_remote_run_opens_no_ip_connection() {
    let world = build_loopback_world();
    let netlog = world.world.path().join("netlog");

    let output = run_gantry_under_observer(&world, &netlog);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "the known-good fixture must round-trip to a pass, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("[gantry] submitted: "),
        "the run must take the remote dispatch path (a local fallback would \
         prove nothing about network confinement), got stderr:\n{stderr}"
    );

    assert_empty_allow_list(&netlog);
}

/// Mutation run: the same observed run, but the fixture now opens a real
/// IP connection from inside the run's own process tree. This is the
/// non-vacuous proof in the direction the positive control cannot cover —
/// the control proves the observer records from a bare child beside the
/// run, this proves a phone-home made anywhere in the run's tree lands in
/// the netlog and trips the empty allow-list with 'phone-home detected'
/// naming the offending peer.
#[test]
fn a_connecting_fixture_is_caught_as_phone_home() {
    let world = build_loopback_world_with_fixture(CONNECTING_FIXTURE_LIB_RS);
    let netlog = world.world.path().join("netlog");

    let output = run_gantry_under_observer(&world, &netlog);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // The mutant is not an error the run can see: its suite still passes
    // and the dispatch still completes, which is exactly why the pledge
    // needs an assertion of its own.
    assert!(
        output.status.success(),
        "the connecting fixture must still round-trip to a pass, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("[gantry] submitted: "),
        "the mutation run must take the remote dispatch path too, got \
         stderr:\n{stderr}"
    );

    // The assertion must trip on what the run recorded.
    let trip = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_empty_allow_list(&netlog);
    }));
    let payload = match trip {
        Ok(()) => panic!(
            "the connecting fixture's real connect must trip the empty \
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
        message.contains("connect dst=127.0.0.1:"),
        "the report must name the syscall and the offending peer the \
         fixture dialed, got: {message}"
    );
}
