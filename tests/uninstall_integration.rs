// gantry — `gantry uninstall` end-to-end (plan §8: "`gantry uninstall`
// reverses it"; bead gantry-b2483624).
//
// Proves the uninstaller through the REAL wiring, not a re-implementation:
// the built binary is copied into a throwaway install tree, a shim symlink
// named `cargo` is laid down ahead of a fake real toolchain on PATH, and
// `gantry uninstall` runs as its own process with a throwaway HOME — the
// same technique as tests/tier0_integration.rs, so the run touches nothing
// outside the fixture and stays hermetic under parallel `cargo test`.
//
// The acceptance the bead names is asserted literally: after a clean
// uninstall, nothing on PATH shadows the real toolchain — the gantry shim
// is gone, the binary is gone, and `cargo` resolves to the real cargo the
// fixture placed behind the shim dir. The copy-instead-of-run dance exists
// because the binary deletes itself: `target/debug/gantry` must survive the
// suite, so the tests always run a disposable copy.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Path to the gantry binary (built with `cargo build --bin gantry`).
fn gantry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gantry"))
}

/// One throwaway install: a copied gantry binary, a shim dir whose `cargo`
/// symlinks to it (ahead of the fake real toolchain on PATH), and a HOME
/// carrying the state dir, user config, and slice unit the writers create.
struct Fixture {
    _home: TempDir,
    _bins: TempDir,
    _shims: TempDir,
    _real: TempDir,
    /// The copied binary the test invokes (it deletes itself on uninstall).
    gantry: PathBuf,
    /// The command env: fixture PATH (shims first), HOME, XDG dirs.
    env: Vec<(String, String)>,
    /// Paths asserted on after the run.
    shim_link: PathBuf,
    real_cargo: PathBuf,
    state_dir: PathBuf,
    config_toml: PathBuf,
    slice_unit: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let bins = TempDir::new().unwrap();
        let shims = TempDir::new().unwrap();
        let real = TempDir::new().unwrap();

        // The disposable copy — the real target/debug/gantry must survive.
        let gantry = bins.path().join("gantry");
        fs::copy(gantry_binary(), &gantry).unwrap();

        // The shim: `[shim dir]/cargo ──symlink──► gantry binary` (plan
        // Components §1), shim dir first on PATH so it shadows the real
        // toolchain — the exact shadow a uninstall must lift.
        let shim_link = shims.path().join("cargo");
        std::os::unix::fs::symlink(&gantry, &shim_link).unwrap();

        // The fake real toolchain: an executable script named cargo behind
        // the shim dir.
        let real_cargo = real.path().join("cargo");
        fs::write(&real_cargo, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&real_cargo, fs::Permissions::from_mode(0o755)).unwrap();

        // The user-level install state, as the writers lay it down: state
        // dir (ledger + kill switch), user config, slice unit.
        let state_dir = home.path().join(".local/state/gantry");
        let config_dir = home.path().join(".config/gantry");
        let unit_dir = home.path().join(".config/systemd/user");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&unit_dir).unwrap();
        fs::write(state_dir.join("runs.jsonl"), "{\"schema_version\":1}\n").unwrap();
        fs::write(state_dir.join("state.toml"), "schema_version = 1\n").unwrap();
        fs::write(config_dir.join("config.toml"), "[remote]\nbackend = \"none\"\n").unwrap();
        let slice_unit = unit_dir.join("gantry.slice");
        fs::write(&slice_unit, "# Managed by gantry\n[Slice]\n").unwrap();

        // PATH carries ONLY the fixture dirs — no systemctl on it, so the
        // daemon-reload degrades to a note (the no-systemd-manager shape)
        // instead of touching the invoking machine's user manager.
        let env = vec![
            (
                "PATH".to_string(),
                format!(
                    "{}:{}",
                    shims.path().display(),
                    real.path().display()
                ),
            ),
            ("HOME".to_string(), home.path().display().to_string()),
            (
                "XDG_CONFIG_HOME".to_string(),
                home.path().join(".config").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                home.path().join(".local/state").display().to_string(),
            ),
        ];

        Fixture {
            _home: home,
            _bins: bins,
            _shims: shims,
            _real: real,
            gantry,
            env,
            shim_link,
            real_cargo,
            state_dir,
            config_toml: config_dir.join("config.toml"),
            slice_unit,
        }
    }

    /// Run `gantry uninstall` (or a flag variant) as a real process.
    fn uninstall(&self, args: &[&str]) -> Output {
        Command::new(&self.gantry)
            .arg("uninstall")
            .args(args)
            .envs(self.env.iter().cloned())
            .output()
            .expect("spawn gantry uninstall")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn uninstall_removes_everything_and_unshadows_the_toolchain() {
    let fx = Fixture::new();
    let out = fx.uninstall(&[]);

    assert!(
        out.status.success(),
        "exit {:?}, stdout:\n{}stderr:\n{}",
        out.status.code(),
        stdout(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("removed shim"), "{text}");
    assert!(text.contains("removed binary"), "{text}");
    assert!(text.contains("removed state directory"), "{text}");
    assert!(text.contains("removed config"), "{text}");
    assert!(text.contains("removed systemd unit"), "{text}");
    assert!(text.contains("gantry uninstall complete"), "{text}");

    // The no-shadowing acceptance, literally: nothing gantry-shaped remains
    // on PATH, and cargo resolves to the real toolchain behind the shim.
    assert!(!fx.shim_link.exists(), "shim symlink must be gone");
    assert!(!fx.gantry.exists(), "binary must be gone");
    assert!(
        text.contains(&format!("cargo now resolves to: {}", fx.real_cargo.display())),
        "{text}"
    );
    assert!(fx.real_cargo.exists(), "the real toolchain must survive");

    // State, config, and slice unit went with it.
    assert!(!fx.state_dir.exists(), "state dir must be gone");
    assert!(!fx.config_toml.exists(), "config must be gone");
    assert!(!fx.slice_unit.exists(), "slice unit must be gone");
}

#[test]
fn dry_run_reports_and_touches_nothing() {
    let fx = Fixture::new();
    let out = fx.uninstall(&["--dry-run"]);

    assert!(out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("would remove shim"), "{text}");
    assert!(text.contains("dry run — nothing was removed"), "{text}");

    // Every artifact still on disk, shim still shadowing.
    assert!(fx.shim_link.exists());
    assert!(fx.gantry.exists());
    assert!(fx.state_dir.exists());
    assert!(fx.config_toml.exists());
    assert!(fx.slice_unit.exists());
}

#[test]
fn keep_config_preserves_the_user_config() {
    let fx = Fixture::new();
    let out = fx.uninstall(&["--keep-config"]);

    assert!(out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("kept config"), "{text}");
    assert!(fx.config_toml.exists(), "--keep-config must keep the config");
    // The rest of the uninstall still happened.
    assert!(!fx.shim_link.exists());
    assert!(!fx.state_dir.exists());
    assert!(!fx.slice_unit.exists());
}

#[test]
fn unknown_flag_is_a_usage_error() {
    let fx = Fixture::new();
    let out = fx.uninstall(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2), "{}", stdout(&out));
    // Nothing was touched on the usage error.
    assert!(fx.shim_link.exists());
    assert!(fx.gantry.exists());
}

#[test]
fn uninstall_over_a_clean_box_is_still_clean() {
    // Run one full uninstall, then invoke a second gantry copy over the
    // leftovers-free tree: no shims, no state, nothing to reverse — and the
    // exit stays 0 (idempotence), with cargo still on the real toolchain.
    let fx = Fixture::new();
    let first = fx.uninstall(&[]);
    assert!(first.status.success(), "{}", stdout(&first));

    let second_dir = TempDir::new().unwrap();
    let second_gantry = second_dir.path().join("gantry");
    fs::copy(gantry_binary(), &second_gantry).unwrap();

    // The first uninstall removed the shim dir's cargo, so PATH's shim slot
    // is inert; the real toolchain dir still resolves.
    let out = Command::new(&second_gantry)
        .arg("uninstall")
        .envs(fx.env.iter().cloned())
        .output()
        .expect("spawn second gantry uninstall");

    assert!(out.status.success(), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("gantry uninstall complete"), "{text}");
    assert!(
        !text.contains("removed shim"),
        "nothing left to remove: {text}"
    );
    assert!(
        text.contains(&format!("cargo now resolves to: {}", fx.real_cargo.display())),
        "{text}"
    );
}
