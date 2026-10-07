//! Integration coverage for `gantry init --ssh` (plan §8, Component 8):
//! the whole onboarding ladder driven through a loopback ssh fixture, plus
//! its failure paths.
//!
//! The fixture is a stand-in `ssh` binary placed first on the spawned
//! process's PATH: it runs the "remote" command in a local sh with HOME and
//! PATH pinned to a throwaway fixture tree, so the executor really is
//! installed into that tree, the preset really is written to the config path
//! the production flow derives from `$HOME`, and the finishing `doctor
//! --e2e` canary is the real round trip. Nothing outside the temp tree is
//! touched: the spawned `gantry init` gets HOME pointed at the fixture, so
//! the config, its backup, and the state dir all land there.
//!
//! The green path's simulated host has git on its PATH and cargo only at
//! `$HOME/.cargo/bin/cargo` — the rustup shape whose whole point is that a
//! non-interactive ssh shell misses it, so the probe's fallback and the
//! wrapper's cargo pin are exercised for real. The canary compile is the
//! same cost `tests/doctor_e2e_integration.rs` already pays.

use gantry::config::{Backend, Config};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The loopback ssh stand-in. argv is `<destination> <remote command…>`;
/// the destination names the loopback and is ignored. Env knobs:
/// `LOOPBACK_SSH_HOME`/`LOOPBACK_SSH_PATH` pin the "remote" side's HOME and
/// PATH (the fixture tree), `LOOPBACK_SSH_MISSING` is a comma list of tools
/// the simulated host lacks, and `LOOPBACK_SSH_UNREACHABLE=1` fails every
/// call the way a dead host does (ssh's own exit 255).
const LOOPBACK_SSH_SH: &str = r#"#!/usr/bin/env sh
# Loopback ssh fixture for the `gantry init --ssh` integration tests.
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
if [ "${LOOPBACK_SSH_UNREACHABLE:-}" = "1" ]; then
    printf 'ssh: connect to host %s port 22: Connection refused\n' "$1" >&2
    exit 255
fi
shift
cmd=$1
case ",${LOOPBACK_SSH_MISSING:-}," in
*,git,*)
    case $cmd in
    'git --version'*) exit 127 ;;
    esac
    ;;
*,cargo,*)
    case $cmd in
    *cargo*) exit 127 ;;
    esac
    ;;
esac
exec "$SH" -c "$cmd"
"#;

/// One loopback world: a temp tree with the stand-in `ssh` on a PATH
/// prepended to the test's own, a simulated host home holding git (on the
/// remote PATH) and, optionally, cargo (rustup-style, off it), and a
/// pre-existing user config the preset must back up.
struct Loopback {
    dir: tempfile::TempDir,
    home: PathBuf,
    config_path: PathBuf,
}

impl Loopback {
    /// `git_on_path`: whether the simulated host can see git at all (the
    /// missing-git failure host cannot).
    fn new(git_on_path: bool) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        // The stand-in ssh lives here, first on the spawned process's PATH.
        let ssh_dir = dir.path().join("ssh-bin");
        fs::create_dir_all(&ssh_dir).unwrap();
        let ssh = ssh_dir.join("ssh");
        fs::write(&ssh, LOOPBACK_SSH_SH).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();

        // The simulated host's PATH dir: git only when the host has it.
        let host_bin = home.join("bin");
        fs::create_dir_all(&host_bin).unwrap();
        if git_on_path {
            symlink_tool("git", &host_bin.join("git"), &home);
        }
        // Minimal-but-functional: the install leg's remote commands (`mkdir
        // -p`, the `cat` upload, `chmod +x`) are ordinary coreutils a real
        // sshd's default PATH always carries, so the fixture carries them
        // too. Cargo stays off this PATH either way — that stripped-PATH
        // rustup shape is the point of the world, not raw hostility.
        for tool in ["mkdir", "cat", "chmod"] {
            symlink_tool(tool, &host_bin.join(tool), &home);
        }

        // A pre-existing user config the preset must back up, not destroy.
        fs::create_dir_all(home.join(".config/gantry")).unwrap();
        let config_path = home.join(".config/gantry/config.toml");
        fs::write(&config_path, "[local]\ncpu_quota_pct = 150\n").unwrap();

        Loopback {
            dir,
            home,
            config_path,
        }
    }

    /// Spawn the real `gantry init` binary against the loopback, with HOME
    /// at the fixture so the production config path resolves inside it.
    fn init(&self, target: &str, host_env: &[(&str, &str)]) -> std::process::Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("ssh-bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_gantry"));
        command
            .args(["init", "--ssh", target])
            .env("HOME", &self.home)
            .env("PATH", &path)
            .env("LOOPBACK_SSH_HOME", &self.home)
            .env("LOOPBACK_SSH_PATH", self.home.join("bin"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME");
        for (key, value) in host_env {
            command.env(key, value);
        }
        command.output().expect("spawn gantry init")
    }
}

/// Symlink `name` into the fixture tree, resolved from the host. The
/// simulated host's PATH deliberately carries no shells, and `#!`
/// interpreter lookup goes through PATH — so a host wrapper script (a
/// `#!/usr/bin/env bash` cargo interceptor, say) that runs fine from the
/// invoking shell dies there with exit 126, which the green path's probe
/// would then report as a found-but-broken toolchain. Link only a
/// candidate that execs standalone, verified the way the probe runs it:
/// under the fixture's HOME with an empty PATH. Cargo prefers the real
/// toolchain binary — the running `CARGO`, then the installed rustup
/// toolchains — over `command -v`'s answer, and the rustup shim is no
/// candidate at all: with the fixture's HOME it would download a toolchain
/// mid-test instead of running one.
fn symlink_tool(name: &str, dest: &Path, exec_home: &Path) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if name == "cargo" {
        if let Ok(cargo) = std::env::var("CARGO") {
            candidates.push(PathBuf::from(cargo));
        }
        let mut rustup_homes: Vec<PathBuf> = Vec::new();
        if let Ok(h) = std::env::var("RUSTUP_HOME") {
            rustup_homes.push(PathBuf::from(h));
        }
        if let Ok(h) = std::env::var("HOME") {
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
    std::os::unix::fs::symlink(source, dest)
        .unwrap_or_else(|e| panic!("symlink {name} -> {}: {e}", dest.display()));
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

/// The cargo file the green host has at the rustup location — off the
/// remote PATH, exactly where the probe's fallback must find it.
fn install_rustup_style_cargo(home: &Path) -> PathBuf {
    let cargo_bin = home.join(".cargo/bin");
    fs::create_dir_all(&cargo_bin).unwrap();
    let dest = cargo_bin.join("cargo");
    symlink_tool("cargo", &dest, home);
    dest
}

fn text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Green path: the full ladder — reach, git (on the host's stripped PATH),
/// cargo (rustup fallback only), install, preset write with backup, and the
/// real `doctor --e2e` canary green at the end.
#[test]
fn init_ssh_onboards_a_loopback_host_end_to_end() {
    let world = Loopback::new(true);
    let cargo = install_rustup_style_cargo(&world.home);

    let output = world.init("ops@loopback.test", &[]);
    let (stdout, stderr) = text(&output);
    assert!(
        output.status.success(),
        "init must succeed; stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The old config was backed up, not destroyed; the live file is the
    // preset — and it parses through the real loader with backend command.
    let backup = world.config_path.with_file_name("config.toml.init-backup");
    assert!(
        backup.exists(),
        "the pre-existing config must be backed up; stdout:\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(&backup).unwrap(),
        "[local]\ncpu_quota_pct = 150\n"
    );
    let loaded = Config::load_layers(None, Some(&world.config_path), None)
        .expect("the written preset must parse");
    assert!(
        loaded.warnings.is_empty(),
        "warnings: {:?}",
        loaded.warnings
    );
    assert_eq!(loaded.config.remote.backend, Backend::Command);
    let command = loaded.config.remote.command.expect("command table");
    let exec = format!("{}/.local/bin/gantry-exec", world.home.display());
    fn argv_of(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    assert_eq!(
        argv_of(&command.submit),
        vec![
            "ssh",
            "ops@loopback.test",
            &exec,
            "submit",
            "{repo}",
            "{rev}",
            "{args_json}",
        ]
    );
    assert_eq!(
        argv_of(&command.wait),
        vec!["ssh", "ops@loopback.test", &exec, "wait", "{handle}"]
    );

    // The executor really landed on the "host", executable, and the wrapper
    // pins the exact rustup path the probe's fallback found — not bare
    // "cargo", which the host's non-interactive shell cannot see.
    let exec_sh = format!("{}/.local/bin/gantry-exec.sh", world.home.display());
    for path in [&exec, &exec_sh] {
        let mode = fs::metadata(path)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{path} must be executable");
    }
    assert!(
        fs::read_to_string(&exec_sh)
            .unwrap()
            .starts_with("#!/usr/bin/env sh"),
        "the reference executor must be the file installed"
    );
    let wrapper = fs::read_to_string(&exec).unwrap();
    assert!(
        wrapper.contains(&format!("GANTRY_EXEC_CARGO='{}'", cargo.display())),
        "the wrapper must pin the probed rustup cargo: {wrapper}"
    );

    // The finishing canary is green, and the transcript reads like doctor's.
    assert!(
        stdout.contains("E2E test: round trip passed"),
        "init must end with a green canary; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ops@loopback.test is ready"),
        "the ready line must name the onboarded host; stdout:\n{stdout}"
    );
}

/// Failure path: a host with git but no cargo anywhere fails the cargo leg
/// with the fix in the message — and writes no config, because
/// verification precedes any write.
#[test]
fn init_ssh_missing_cargo_fails_before_writing_any_config() {
    let world = Loopback::new(true);

    let output = world.init("ops@loopback.test", &[("LOOPBACK_SSH_MISSING", "cargo")]);
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed verification leg exits 1; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("leg 'cargo' failed"),
        "the message must name the leg: {stderr}"
    );
    assert!(
        stderr.contains("rustup"),
        "the message must carry the fix: {stderr}"
    );
    assert!(
        stderr.contains("on ops@loopback.test"),
        "the message must name the target the fix applies to: {stderr}"
    );
    assert!(
        stderr.contains("re-run gantry init --ssh ops@loopback.test"),
        "the message must end in the re-run command for the target: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&world.config_path).unwrap(),
        "[local]\ncpu_quota_pct = 150\n",
        "no config may be written when verification fails — the pre-existing \
         one must survive byte-exact"
    );
    assert!(
        !world
            .config_path
            .with_file_name("config.toml.init-backup")
            .exists(),
        "a failed verification leg must not back the previous config aside either"
    );
    assert!(
        !world.home.join(".local/bin").exists(),
        "the executor must not be installed past a failed verification leg"
    );
}

/// Failure path: a host without git names the git leg and writes nothing.
#[test]
fn init_ssh_missing_git_names_the_git_leg() {
    let world = Loopback::new(false);

    let output = world.init("ops@loopback.test", &[("LOOPBACK_SSH_MISSING", "git")]);
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.contains("leg 'git' failed"),
        "the message must name the leg: {stderr}"
    );
    assert!(
        stderr.contains("install git"),
        "the message must carry the fix: {stderr}"
    );
    assert!(
        stderr.contains("on ops@loopback.test"),
        "the message must name the target the fix applies to: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&world.config_path).unwrap(),
        "[local]\ncpu_quota_pct = 150\n",
        "no config may be written when verification fails — the pre-existing \
         one must survive byte-exact"
    );
    assert!(
        !world
            .config_path
            .with_file_name("config.toml.init-backup")
            .exists(),
        "a failed verification leg must not back the previous config aside either"
    );
}

/// Failure path: an unreachable host fails the reach leg with the
/// connectivity checklist, not a toolchain hint.
#[test]
fn init_ssh_unreachable_host_fails_the_reach_leg() {
    let world = Loopback::new(true);

    let output = world.init("ops@loopback.test", &[("LOOPBACK_SSH_UNREACHABLE", "1")]);
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.contains("leg 'reach' failed"),
        "the message must name the leg: {stderr}"
    );
    assert!(
        stderr.contains("cannot reach ops@loopback.test over ssh"),
        "an ssh-transport failure must read as connectivity, not a missing tool: {stderr}"
    );
    assert!(
        stderr.contains("`ssh ops@loopback.test true`"),
        "the remedy must be the paste-able probe naming the target: {stderr}"
    );
    // The fixture pre-creates a config (the backup tests need one), so the
    // assertion is survival, not nonexistence: the unreachable host must
    // leave it byte-exact and must not back it aside either.
    assert_eq!(
        fs::read_to_string(&world.config_path).unwrap(),
        "[local]\ncpu_quota_pct = 150\n",
        "no config may be written when the host cannot be reached — the \
         pre-existing one must survive byte-exact"
    );
    assert!(
        !world
            .config_path
            .with_file_name("config.toml.init-backup")
            .exists(),
        "an unreachable host must not back the previous config aside"
    );
}

/// Usage errors exit 2 through the real binary, before anything runs — the
/// parse is rejected before the config directory is even resolved, so the
/// run leaves no config side effect behind it either.
#[test]
fn init_usage_errors_exit_two() {
    let home = tempfile::TempDir::new().unwrap();
    for args in [
        vec!["init"],
        vec!["init", "--ssh"],
        vec!["init", "--bogus"],
        vec!["init", "--ssh", "bad target"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gantry"))
            .args(&args)
            .env("HOME", home.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn gantry {args:?}: {e}"));
        assert_eq!(
            output.status.code(),
            Some(2),
            "gantry {args:?} must be a usage error; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        !home.path().join(".config/gantry").exists(),
        "a usage error must not create any config side effect"
    );
}
