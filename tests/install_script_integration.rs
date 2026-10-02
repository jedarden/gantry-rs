// gantry — install.sh end-to-end (plan Phase 3 acceptance: "curl-pipe install
// on a fresh box reaches a passing `quickcheck` with zero config"; bead
// bf-3qm).
//
// The tests drive the REAL `install.sh` — the same bytes the curl-pipe
// serves — as its own process in a throwaway HOME, never a re-implementation
// of its logic. Two layers are covered:
//
// 1. the **download path**, against a localhost HTTP server that speaks the
//    exact URL shapes of the production forge (release-latest lookup,
//    tarball, checksums.txt): fetch, checksum-verify, unpack, lay the binary
//    + shim, and finish with a passing `quickcheck` — the acceptance, minus
//    only the network's transport;
// 2. the **local-file path** (`GANTRY_INSTALL_LOCAL_BIN`) plus the guard
//    rails: a foreign `cargo` is refused (and moved aside only by --force),
//    a checksum mismatch aborts before anything is installed, the
//    uninstall/doctor verbs delegate to the binary, and an unknown verb is a
//    usage error.
//
// Hermeticity is the same technique as tests/uninstall_integration.rs: a
// throwaway HOME, a throwaway install dir, and a fake real toolchain behind
// the shim dir, so nothing outside the fixture is touched and parallel
// `cargo test` stays safe. The quickchecks inside these installs run in the
// documented degrade tier (no systemd user bus in the child env), which is
// exactly the tier a container or CI box gets — the tier that must pass with
// zero config.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use tempfile::TempDir;

/// install.sh at the repo root — the file the curl-pipe serves.
fn install_sh() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// A throwaway install world: a HOME, the install dir the script derives
/// from it (`$HOME/.local/bin`), and a fake real toolchain the shim must
/// resolve behind that dir.
struct Fixture {
    _home: TempDir,
    real: TempDir,
    /// $HOME/.local/bin — install.sh's default GANTRY_BIN_DIR.
    bin_dir: PathBuf,
    /// Child env: fixture HOME, shim dir + fake toolchain first on PATH.
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let real = TempDir::new().unwrap();

        // The fake real toolchain: an executable script named cargo behind
        // the shim dir (the same shape the uninstall fixture uses).
        let real_cargo = real.path().join("cargo");
        fs::write(&real_cargo, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&real_cargo, fs::Permissions::from_mode(0o755)).unwrap();

        let bin_dir = home.path().join(".local/bin");
        let mut path = String::new();
        for dir in [bin_dir.clone(), real.path().to_path_buf()] {
            path.push_str(dir.to_str().unwrap());
            path.push(':');
        }
        path.push_str(&std::env::var("PATH").unwrap());
        let env = vec![
            ("HOME".to_string(), home.path().display().to_string()),
            ("PATH".to_string(), path),
        ];
        Fixture {
            _home: home,
            real,
            bin_dir,
            env,
        }
    }

    fn run_install(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg(install_sh()).args(args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().expect("install.sh must spawn")
    }

    /// Run the installed binary itself under the same fixture env.
    fn run_gantry(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(self.bin_dir.join("gantry"));
        cmd.args(args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.output().expect("installed gantry must spawn")
    }
}

fn assert_success(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} must succeed; stderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
}

/// sha256 of $path as bare hex — the same dual support install.sh's verifier
/// has (coreutils sha256sum, BSD shasum fallback).
fn sha256_of(path: &Path) -> String {
    for (prog, args) in [
        ("sha256sum", vec![]),
        ("shasum", vec!["-a".to_string(), "256".to_string()]),
    ] {
        if let Ok(out) = Command::new(prog).args(args).arg(path).output() {
            if out.status.success() {
                let line = String::from_utf8_lossy(&out.stdout);
                return line.split_whitespace().next().unwrap().to_string();
            }
        }
    }
    panic!("neither sha256sum nor shasum is available");
}

/// Stage a release layout on disk and serve it over HTTP with the production
/// forge's URL shapes:
///
/// - `GET /api/v1/repos/<repo>/releases/latest` — the tag lookup install.sh
///   parses `"tag_name"` out of;
/// - `GET /<repo>/releases/download/<tag>/<artifact>` — tarball and
///   checksums.txt.
///
/// Returns the base URL for `GANTRY_INSTALL_FORGE`. The checksum manifest can
/// be corrupted by the caller to exercise the mismatch abort.
struct ReleaseServer {
    base: String,
    /// Kept so the accept loop lives as long as the test (a detached thread
    /// would equally do; the handle makes the lifetime explicit).
    _handle: thread::JoinHandle<()>,
}

fn build_tarball(bin: &Path, dest: &Path) {
    let stage = dest.parent().unwrap().join(".tarstage");
    fs::create_dir_all(&stage).unwrap();
    fs::copy(bin, stage.join("gantry")).unwrap();
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(dest)
        .arg("-C")
        .arg(&stage)
        .arg("gantry")
        .status()
        .expect("tar must spawn");
    assert!(status.success(), "tar failed staging the release tarball");
    fs::remove_dir_all(&stage).unwrap();
}

fn serve_release(bin: &Path, tag: &str, corrupt_checksum: bool) -> ReleaseServer {
    let tarball_path = bin
        .parent()
        .unwrap()
        .join("gantry-x86_64-linux-musl.tar.gz");
    build_tarball(bin, &tarball_path);
    let tarball = fs::read(&tarball_path).unwrap();

    // The manifest checksums the artifacts as published — the tarball's own
    // digest (not the binary's), plus install.sh's, exactly the layout
    // package-release.sh writes.
    let mut checksums = format!(
        "{}  gantry-x86_64-linux-musl.tar.gz\n{}  install.sh\n",
        sha256_of(&tarball_path),
        sha256_of(&install_sh()),
    );
    if corrupt_checksum {
        checksums = format!("{}  gantry-x86_64-linux-musl.tar.gz\n", "0".repeat(64));
    }
    let _ = fs::remove_file(&tarball_path);
    let checksums = checksums.into_bytes();
    let tag = tag.to_string();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Read the request head (GET has no body): until the header
            // terminator or the buffer fills.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf);
            let path = req.split_whitespace().nth(1).unwrap_or("").to_string();

            // The two route shapes install.sh fetches; anything else is 404
            // (curl -f then fails the install — the behavior a forge outage
            // must produce).
            let latest = "/api/v1/repos/jedarden/gantry-rs/releases/latest".to_string();
            let download_prefix = format!("/jedarden/gantry-rs/releases/download/{tag}/");
            let body: Vec<u8> = if path == latest {
                format!("{{\"tag_name\":\"{tag}\",\"name\":\"gantry {tag}\"}}\n").into_bytes()
            } else if let Some(artifact) = path.strip_prefix(&download_prefix) {
                match artifact {
                    "gantry-x86_64-linux-musl.tar.gz" => tarball.clone(),
                    "checksums.txt" => checksums.clone(),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let found = !body.is_empty();
            let status = if found { "200 OK" } else { "404 Not Found" };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });

    ReleaseServer {
        base: format!("http://127.0.0.1:{port}"),
        _handle: handle,
    }
}

/// The install-completed assertions shared by the download and local-file
/// paths: binary laid, shim symlink pointing at it, quickcheck passed with
/// zero config (the acceptance, verbatim in its effect).
fn assert_installed(fx: &Fixture) {
    assert!(
        fx.bin_dir.join("gantry").is_file(),
        "binary must be installed"
    );
    let link = fx.bin_dir.join("cargo");
    assert!(
        fs::symlink_metadata(&link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "shim must be a symlink at {:?}",
        link
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("gantry"),
        "shim target"
    );

    // The installed binary itself must pass quickcheck under the fixture env
    // — the install script's gate re-proven against the installed artifact
    // (zero config: the fixture wrote none).
    let qc = fx.run_gantry(&["quickcheck"]);
    assert_success(&qc, "gantry quickcheck after install");
    let qc_out = String::from_utf8_lossy(&qc.stdout);
    assert!(
        qc_out.contains("quickcheck: passed"),
        "quickcheck must pass with zero config; stdout:\n{qc_out}"
    );
}

#[test]
fn curl_pipe_install_downloads_verifies_and_reaches_passing_quickcheck() {
    let fx = Fixture::new();
    // A disposable copy — the fixture's bin_dir may be wiped by the install.
    let bin = fx.real.path().join("gantry-release-copy");
    fs::copy(gantry_binary(), &bin).unwrap();

    let server = serve_release(&bin, "v0.1.0", false);

    // No GANTRY_INSTALL_VERSION: the tag comes from the release-latest
    // lookup, exactly the bare curl-pipe's shape.
    let forge = ("GANTRY_INSTALL_FORGE", server.base.as_str());
    let out = fx.run_install(&[], &[forge]);
    assert_success(&out, "curl-pipe install");

    // The tag was resolved from the forge (the install names it in its
    // download line).
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("v0.1.0"),
        "install must download the resolved tag; stdout:\n{stdout}"
    );
    assert_installed(&fx);
}

#[test]
fn checksum_mismatch_aborts_before_anything_is_installed() {
    let fx = Fixture::new();
    let bin = fx.real.path().join("gantry-release-copy");
    fs::copy(gantry_binary(), &bin).unwrap();

    let server = serve_release(&bin, "v0.1.0", true);

    let forge = ("GANTRY_INSTALL_FORGE", server.base.as_str());
    let version = ("GANTRY_INSTALL_VERSION", "v0.1.0");
    let out = fx.run_install(&[], &[forge, version]);
    assert!(
        !out.status.success(),
        "a corrupt download must fail the install"
    );
    assert!(
        !fx.bin_dir.join("gantry").exists(),
        "nothing may be installed on a checksum mismatch"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checksum mismatch"),
        "the failure must name the cause; stderr:\n{stderr}"
    );
}

#[test]
fn local_file_install_reaches_passing_quickcheck_with_zero_config() {
    let fx = Fixture::new();
    // A disposable copy, as the hermetic-integration escape hatch ships.
    let bin = fx.real.path().join("gantry-local-copy");
    fs::copy(gantry_binary(), &bin).unwrap();
    let local = (
        "GANTRY_INSTALL_LOCAL_BIN",
        bin.to_str().unwrap().to_string(),
    );
    let out = fx.run_install(&[], &[(local.0, local.1.as_str())]);
    assert_success(&out, "local-file install");
    assert_installed(&fx);

    // Zero config: the fixture never wrote anything under ~/.config/gantry.
    assert!(
        !fx._home.path().join(".config/gantry").exists(),
        "install must not write configuration (Tier-0 contract)"
    );
}

#[test]
fn install_refuses_to_replace_a_foreign_cargo() {
    let fx = Fixture::new();
    let bin = fx.real.path().join("gantry-local-copy");
    fs::copy(gantry_binary(), &bin).unwrap();

    // A foreign toolchain occupies the shim name.
    fs::create_dir_all(&fx.bin_dir).unwrap();
    let foreign = fx.bin_dir.join("cargo");
    fs::write(&foreign, "#!/bin/sh\necho foreign-toolchain\n").unwrap();
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o755)).unwrap();

    let local = (
        "GANTRY_INSTALL_LOCAL_BIN",
        bin.to_str().unwrap().to_string(),
    );
    let out = fx.run_install(&[], &[(local.0, local.1.as_str())]);
    assert!(
        !out.status.success(),
        "install must refuse to clobber a foreign cargo"
    );
    let on_disk = fs::read_to_string(&foreign).unwrap();
    assert_eq!(
        on_disk, "#!/bin/sh\necho foreign-toolchain\n",
        "foreign file untouched"
    );
    assert!(
        !fx.bin_dir.join("gantry").exists(),
        "a refused install lays no binary"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing"),
        "the refusal must be named; stderr:\n{stderr}"
    );
}

#[test]
fn install_force_moves_a_foreign_cargo_aside_instead_of_deleting_it() {
    let fx = Fixture::new();
    let bin = fx.real.path().join("gantry-local-copy");
    fs::copy(gantry_binary(), &bin).unwrap();

    fs::create_dir_all(&fx.bin_dir).unwrap();
    let foreign = fx.bin_dir.join("cargo");
    fs::write(&foreign, "#!/bin/sh\necho foreign-toolchain\n").unwrap();
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o755)).unwrap();

    let local = (
        "GANTRY_INSTALL_LOCAL_BIN",
        bin.to_str().unwrap().to_string(),
    );
    let out = fx.run_install(&["install", "--force"], &[(local.0, local.1.as_str())]);
    assert_success(&out, "forced install");

    // The foreign toolchain is kept, not deleted — the .bak convention.
    let bak = fx.bin_dir.join("cargo.pre-gantry.bak");
    assert_eq!(
        fs::read_to_string(&bak).unwrap(),
        "#!/bin/sh\necho foreign-toolchain\n",
        "the moved-aside file keeps its bytes"
    );
    assert_installed(&fx);
}

#[test]
fn uninstall_verb_delegates_to_the_binary_and_reverses_the_layout() {
    let fx = Fixture::new();
    let bin = fx.real.path().join("gantry-local-copy");
    fs::copy(gantry_binary(), &bin).unwrap();
    let local = (
        "GANTRY_INSTALL_LOCAL_BIN",
        bin.to_str().unwrap().to_string(),
    );
    assert_success(
        &fx.run_install(&[], &[(local.0, local.1.as_str())]),
        "install",
    );

    let out = fx.run_install(&["uninstall"], &[]);
    assert_success(&out, "install.sh uninstall");
    assert!(
        !fx.bin_dir.join("gantry").exists() && !fx.bin_dir.join("cargo").exists(),
        "the uninstall must remove the binary and the shim"
    );
}

#[test]
fn doctor_verb_relays_the_binary_exit_code() {
    let fx = Fixture::new();
    let bin = fx.real.path().join("gantry-local-copy");
    fs::copy(gantry_binary(), &bin).unwrap();
    let local = (
        "GANTRY_INSTALL_LOCAL_BIN",
        bin.to_str().unwrap().to_string(),
    );
    assert_success(
        &fx.run_install(&[], &[(local.0, local.1.as_str())]),
        "install",
    );

    // The script is a delegate: its exit code must be exactly what the
    // binary's doctor returns under the same env — whatever that is.
    let via_script = fx.run_install(&["doctor"], &[]);
    let direct = fx.run_gantry(&["doctor"]);
    assert_eq!(
        via_script.status.code(),
        direct.status.code(),
        "install.sh doctor must relay gantry doctor's own exit code"
    );
}

#[test]
fn unknown_verb_is_a_usage_error() {
    let fx = Fixture::new();
    let out = fx.run_install(&["frobnicate"], &[]);
    assert_eq!(out.status.code(), Some(2), "unknown verb must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:"),
        "usage must be printed; stderr:\n{stderr}"
    );
}
