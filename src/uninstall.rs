// gantry — `gantry uninstall`: reverse an install (plan §8 "doctor / installer":
// "`gantry uninstall` reverses it"; CLI surface: "gantry install-shims |
// uninstall"; features.md "Installer + uninstaller").
//
// The install layout uninstall reverses is the one the rest of the codebase
// already defines — the same paths the writers use, never a second spelling:
//
// - the binary, `~/.local/bin/gantry` (plan §8 install.sh line), located as
//   the running executable's canonical path;
// - the shim symlink(s) named `cargo` in PATH directories whose target is
//   the gantry binary (plan Components §1 diagram: `[shim dir]/cargo
//   ──symlink──► gantry binary`) — plus, defensively, content-identical
//   *copies* of the gantry binary named `cargo`, which shadow the real
//   toolchain just as hard;
// - the state directory, `~/.local/state/gantry/` — the write-ahead run
//   ledger (`runs.jsonl`), the on/off kill-switch file (`state.toml`), the
//   LKG config snapshot, and any REDACTED crash bundles (plan Component 7);
// - the user config, `~/.config/gantry/config.toml` (plan Component 3
//   three-layer config, layer 2) — `--keep-config` preserves it;
// - the systemd user slice unit, `gantry.slice` under the user unit dir
//   (plan Component 6), followed by a `systemctl --user daemon-reload`.
//
// Reported, never touched: `/etc/gantry/config.toml` (layer 1 — needs root,
// and a per-user uninstall must not sudo) and any repo-local `.gantry.toml`
// (layer 3 — owned by the repo, not the install).
//
// The safety rule that governs everything here: **uninstall must never delete
// the real cargo.** Only a PATH entry that is provably gantry's — a symlink
// resolving to the gantry binary, a dangling symlink whose link text still
// names it, or a regular file byte-identical to it — qualifies for removal;
// anything else named `cargo` on PATH is someone else's toolchain and is
// left alone. The binary itself is deleted only when its file name is
// `gantry`, so a gantry copy masquerading as `cargo` is reported for manual
// removal rather than deleted by rule (the PATH scan skips the running
// binary's own path — that file belongs to the binary step, not the shim
// step).
//
// Structure is the same pure/effect split the rest of gantry uses:
// [`execute`] does filesystem work against *explicit* paths (no environment
// reads, no `current_exe`), which is what makes the full uninstall sequence
// unit-testable against temp trees without touching the developer's real
// HOME or the test binary itself; [`cli`] derives the real targets
// (`current_exe`, `dirs`, `$PATH`), parses flags, prints the report, and
// owns the one process spawn (`daemon-reload`) so `execute` stays fs-only.
//
// Exit codes follow the management-CLI convention (report/why/status): 0 a
// clean uninstall, 1 completed with leftovers needing manual attention, 2
// usage error. No prompts — gantry is agent-first, and a prompt in an
// uninstall path hangs exactly the fleet this tool serves.

use std::fs;
use std::path::{Path, PathBuf};

/// The file names a gantry shim may sit under on PATH (plan Components §1:
/// the shim is "the binary installed under the name `cargo`"; the `.exe`
/// spelling covers a Windows install, matching `invocation_name`'s stance).
const SHIM_NAMES: [&str; 2] = ["cargo", "cargo.exe"];

/// File names the gantry *binary* itself is allowed to carry for deletion.
const BINARY_NAMES: [&str; 2] = ["gantry", "gantry.exe"];

/// The explicit uninstall targets, derived once by the CLI wrapper (or
/// crafted by tests) and consumed by [`execute`]. Every field is a path
/// [`execute`] is allowed to remove — there are no environment reads inside
/// `execute`, so a test drives the exact same code the binary runs.
#[derive(Debug, Clone)]
pub struct UninstallTargets {
    /// The gantry binary's canonical path — the file to delete. Derived from
    /// `std::env::current_exe().canonicalize()` in production.
    pub self_binary: PathBuf,
    /// The pre-canonicalization executable path when it is a *distinct*
    /// symlink onto [`Self::self_binary`] (a wrapper-symlink install); such a
    /// wrapper dangles once the binary is gone, so it is removed with it.
    /// `None` when the executable path already is the binary (the Linux
    /// `current_exe` case, which resolves through symlinks itself).
    pub self_wrapper: Option<PathBuf>,
    /// Every state directory gantry's writers might have used: the ledger
    /// and kill switch live at the HOME-based path (`state.rs`), the LKG
    /// snapshot at the XDG-aware one (`config.rs`), and the two coincide
    /// unless `XDG_STATE_HOME` is set — sweep both, deduped.
    pub state_dirs: Vec<PathBuf>,
    /// The gantry user-config *directory* (`~/.config/gantry`), holding
    /// `config.toml` (plan Component 3 layer 2).
    pub config_dir: Option<PathBuf>,
    /// The systemd user slice unit file (`…/systemd/user/gantry.slice`,
    /// plan Component 6), when the platform user-unit dir is determinable.
    pub unit_path: Option<PathBuf>,
}

/// Uninstall options (the `gantry uninstall` flags).
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Classify and report everything, remove nothing.
    pub dry_run: bool,
    /// Preserve the user config (`~/.config/gantry/config.toml`) — for
    /// reinstalls that want to keep backend settings.
    pub keep_config: bool,
}

/// What one uninstall run found and did. Every field renders into the
/// report; `leftovers` is the only thing that changes the exit code.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// The run was a dry run: `removed_*` items were classified, not touched.
    pub dry_run: bool,
    /// Shim symlinks (and content-identical copies) named `cargo` removed
    /// from PATH directories.
    pub removed_shims: Vec<PathBuf>,
    /// The gantry binary, when it carried a `gantry` name and was removed.
    pub removed_binary: Option<PathBuf>,
    /// A distinct wrapper symlink onto the binary, removed with it.
    pub removed_wrapper: Option<PathBuf>,
    /// State directories removed (ledger, kill switch, LKG, crash bundles).
    pub removed_state_dirs: Vec<PathBuf>,
    /// The user config file removed (`--keep-config` absent).
    pub removed_config: Option<PathBuf>,
    /// The systemd slice unit removed.
    pub removed_unit: Option<PathBuf>,
    /// The user config preserved by `--keep-config`.
    pub kept_config: Option<PathBuf>,
    /// Informational lines (kept directories, degraded steps).
    pub notes: Vec<String>,
    /// Gantry artifacts that remain and need manual attention — the only
    /// field that turns the exit code non-zero.
    pub leftovers: Vec<String>,
    /// Where `cargo` resolves on PATH after the removal, when anything
    /// does — the positive half of the no-shadowing check.
    pub cargo_resolves_to: Option<PathBuf>,
}

impl Outcome {
    /// A clean uninstall: nothing gantry-shaped remains on PATH or disk.
    pub fn is_clean(&self) -> bool {
        self.leftovers.is_empty()
    }
}

/// Classify one PATH-dir entry named `cargo`/`cargo.exe`: is it provably a
/// gantry artifact?
///
/// Three shapes match:
/// 1. a symlink whose target (relative link text is joined to the link's
///    directory first) canonicalizes to the gantry binary;
/// 2. a *dangling* symlink whose link text — resolved against the link's
///    directory — names the gantry binary path anyway (the binary-deleted-
///    first state, or a second uninstall run);
/// 3. a regular file byte-identical to the gantry binary (a copy install
///    shadows the toolchain exactly like a symlink does).
///
/// Anything else — the real cargo, an unrelated symlink, a directory, a
/// non-identical file — is *not* a gantry artifact and is never touched.
/// `self_alt` is the pre-canonicalization binary path, matched textually
/// for dangling links whose target cannot be canonicalized. Never panics:
/// every stat/read failure classifies as "not a gantry artifact".
fn is_gantry_artifact(entry: &Path, self_binary: &Path, self_alt: &Path) -> bool {
    // A regular file can only match by content. `entry` may itself be a
    // symlink onto a file, so the symlink check must come first.
    let Ok(md) = fs::symlink_metadata(entry) else {
        return false;
    };
    if !md.file_type().is_symlink() {
        return md.is_file() && files_identical(entry, self_binary);
    }

    // Symlink: resolve the link text against the link's own directory, then
    // try canonicalization (the live-link case); a link that will not
    // canonicalize is matched textually (the dangling case).
    let Ok(link_text) = fs::read_link(entry) else {
        return false;
    };
    let joined = entry
        .parent()
        .map(|dir| dir.join(&link_text))
        .unwrap_or_else(|| link_text.clone());
    match joined.canonicalize() {
        Ok(resolved) => resolved == self_binary,
        Err(_) => joined == self_binary || joined == self_alt,
    }
}

/// Whether two files exist and have identical bytes (length-checked first —
/// a size mismatch short-circuits without reading). This is what lets the
/// scan recognize a *copied* gantry named `cargo` as gantry's: nothing else
/// on a box is byte-identical to the binary. Never panics; unreadable files
/// simply do not match.
fn files_identical(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() {
        return false;
    }
    let (Ok(data_a), Ok(data_b)) = (fs::read(a), fs::read(b)) else {
        return false;
    };
    data_a == data_b
}

/// Scan a PATH-style string for gantry artifacts named `cargo`/`cargo.exe`.
///
/// Returns the matching entries in PATH order. Empty PATH entries (the
/// CWD slot) are skipped: an uninstall must not classify `./cargo` in
/// whatever directory it happened to start in, and a relative entry is not
/// a place an install lives. Nonexistent directories are skipped silently —
/// a stale PATH slot is not a finding.
fn scan_path(path_var: &str, self_binary: &Path, self_alt: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        for name in SHIM_NAMES {
            let candidate = Path::new(dir).join(name);
            if is_gantry_artifact(&candidate, self_binary, self_alt) {
                found.push(candidate);
            }
        }
    }
    found
}

/// Where `cargo` resolves on PATH once every gantry artifact is gone (or was
/// never there): the first entry whose `cargo` is an executable file that is
/// *not* a gantry artifact. Gantry-shaped entries are passed over, not
/// followed — this is `find_in_path`'s walk with uninstall's classification
/// bolted on, so the answer never lands back on gantry. `None` means no
/// non-gantry `cargo` is reachable from this PATH — reported as
/// informational, not failure (the real toolchain may live on a login
/// shell's PATH, e.g. behind rustup's env).
fn resolve_cargo_after(path_var: &str, self_binary: &Path, self_alt: &Path) -> Option<PathBuf> {
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        for name in SHIM_NAMES {
            let candidate = Path::new(dir).join(name);
            if is_gantry_artifact(&candidate, self_binary, self_alt) {
                continue;
            }
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `candidate` is an existing executable regular file (symlinks
/// followed). The same notion of "executable" `find_in_path` applies — Unix
/// mode bits, Windows extensions — kept local because the shim module's
/// helpers are private by design and uninstall's contract differs (it must
/// *see* dangling links, which a PATH lookup skips).
fn is_executable_file(candidate: &Path) -> bool {
    let Ok(md) = fs::metadata(candidate) else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        matches!(
            candidate.extension().and_then(|e| e.to_str()),
            Some("exe") | Some("bat") | Some("cmd") | Some("com")
        )
    }
}

/// Run the uninstall sequence against explicit targets.
///
/// Order matters: shims are classified *before* the binary is deleted (the
/// classification needs the live binary for content comparison and
/// canonicalization), then the binary, wrapper, state dirs, config, and
/// slice unit, and only then is PATH re-walked for the report.
///
/// Removals are best-effort and every failure lands in
/// [`Outcome::leftovers`] with the reason — an uninstall that could not
/// finish says so per item rather than failing the whole run or pretending
/// it succeeded. Idempotent: a second run over a clean box removes nothing
/// and still reports clean.
pub fn execute(targets: &UninstallTargets, path_var: &str, opts: &Options) -> Outcome {
    let mut out = Outcome {
        dry_run: opts.dry_run,
        ..Outcome::default()
    };
    // The pre-canonicalization binary path: the dangling-link matcher's
    // textual comparison target, and the wrapper step's own subject.
    let self_alt = targets
        .self_wrapper
        .as_ref()
        .unwrap_or(&targets.self_binary);

    // 1. Shims — classify everything first (while the binary is still there
    //    to compare against), remove after. The running binary's own path is
    //    excluded: when gantry runs under a foreign name, that file belongs
    //    to the guarded binary step below, not to the shim sweep.
    let shims: Vec<PathBuf> = scan_path(path_var, &targets.self_binary, self_alt)
        .into_iter()
        .filter(|p| p != &targets.self_binary)
        .collect();
    if opts.dry_run {
        out.removed_shims = shims;
    } else {
        for shim in shims {
            match fs::remove_file(&shim) {
                Ok(()) => out.removed_shims.push(shim),
                Err(e) => out.leftovers.push(format!(
                    "leftover: could not remove shim {}: {e}",
                    shim.display()
                )),
            }
        }
    }

    // 2. The binary — only under a `gantry` name. A gantry copy installed as
    //    `cargo` is the one shape rule-based deletion must not touch: it is
    //    reported for manual removal instead (the shim scan above already
    //    removed the content-identical `cargo` entries from PATH dirs —
    //    except the running file itself, which is this step's subject).
    if BINARY_NAMES.contains(
        &targets
            .self_binary
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(""),
    ) {
        if targets.self_binary.is_file() {
            if opts.dry_run {
                out.removed_binary = Some(targets.self_binary.clone());
            } else if let Err(e) = fs::remove_file(&targets.self_binary) {
                out.leftovers.push(format!(
                    "leftover: could not remove binary {}: {e}",
                    targets.self_binary.display()
                ));
            } else {
                out.removed_binary = Some(targets.self_binary.clone());
            }
        }
    } else if targets.self_binary.is_file() {
        out.leftovers.push(format!(
            "leftover: the gantry binary {} does not carry a gantry name; remove it manually",
            targets.self_binary.display()
        ));
    }

    // 2b. A wrapper symlink onto the binary (distinct pre-canonicalization
    //     executable path) dangles once the binary is gone — remove it too.
    if let Some(wrapper) = &targets.self_wrapper {
        if wrapper != &targets.self_binary
            && is_gantry_artifact(wrapper, &targets.self_binary, wrapper)
        {
            if opts.dry_run {
                out.removed_wrapper = Some(wrapper.clone());
            } else if let Err(e) = fs::remove_file(wrapper) {
                out.leftovers.push(format!(
                    "leftover: could not remove {}: {e}",
                    wrapper.display()
                ));
            } else {
                out.removed_wrapper = Some(wrapper.clone());
            }
        }
    }

    // 3. State directories — the whole dir: ledger, kill switch, LKG,
    //    crash bundles all live inside (plan Component 7).
    for state_dir in &targets.state_dirs {
        if !state_dir.is_dir() {
            continue;
        }
        if opts.dry_run {
            out.removed_state_dirs.push(state_dir.clone());
        } else if let Err(e) = fs::remove_dir_all(state_dir) {
            out.leftovers.push(format!(
                "leftover: could not remove state directory {}: {e}",
                state_dir.display()
            ));
        } else {
            out.removed_state_dirs.push(state_dir.clone());
        }
    }

    // 4. User config — the file, then the dir if it empties. `--keep-config`
    //    records the preservation instead.
    if let Some(config_dir) = &targets.config_dir {
        let config_file = config_dir.join("config.toml");
        if opts.keep_config {
            if config_file.is_file() {
                out.kept_config = Some(config_file);
            }
        } else if config_file.is_file() {
            if opts.dry_run {
                out.removed_config = Some(config_file);
            } else {
                match fs::remove_file(&config_file).and_then(|()| fs::remove_dir(config_dir)) {
                    Ok(()) => out.removed_config = Some(config_file),
                    Err(e) if config_file.exists() => out.leftovers.push(format!(
                        "leftover: could not remove config {}: {e}",
                        config_file.display()
                    )),
                    // The file went but the dir kept other files: leave the
                    // dir, it is no longer gantry's alone.
                    Err(_) => out.notes.push(format!(
                        "note: kept {} (contains other files)",
                        config_dir.display()
                    )),
                }
            }
        }
    }

    // 5. The systemd slice unit (plan Component 6). The caller (cli) runs the
    //    daemon-reload — execute stays filesystem-only.
    if let Some(unit) = &targets.unit_path {
        if unit.is_file() {
            if opts.dry_run {
                out.removed_unit = Some(unit.clone());
            } else if let Err(e) = fs::remove_file(unit) {
                out.leftovers.push(format!(
                    "leftover: could not remove systemd unit {}: {e}",
                    unit.display()
                ));
            } else {
                out.removed_unit = Some(unit.clone());
            }
        }
    }

    // 6. Verify: re-walk PATH for anything gantry-shaped that survived (a
    //    failed removal, or an artifact in a dir the first scan could not
    //    see), and resolve where `cargo` lands now — the positive half of
    //    "no shim directory shadowing the real toolchain".
    //
    //    The survivor sweep runs on real runs only: a dry run removed
    //    nothing, so every classified artifact is still on PATH by design —
    //    reporting it as a leftover would make --dry-run exit 1 precisely
    //    when the uninstall it previews would be clean.
    //
    //    The content-identical copy check needs the live binary; once the
    //    binary is gone, surviving symlinks are still caught by link-text
    //    matching, and a surviving *copy* (removal failed while the binary's
    //    succeeded) is caught here only when the binary still exists — the
    //    first scan already handled the normal case.
    if !opts.dry_run {
        let survivors = scan_path(path_var, &targets.self_binary, self_alt);
        for survivor in survivors {
            out.leftovers.push(format!(
                "leftover: gantry shim still on PATH: {}",
                survivor.display()
            ));
        }
    }
    out.cargo_resolves_to = resolve_cargo_after(path_var, &targets.self_binary, self_alt);

    out
}

/// The `gantry uninstall` CLI: parse flags, derive the real targets from the
/// process (executable, HOME/XDG dirs, `$PATH`), run [`execute`], print the
/// report, reload the systemd user manager if the slice unit went away.
///
/// Returns the process exit code: 0 clean, 1 leftovers remain, 2 usage
/// error. Never panics — every target derivation failure becomes a report
/// line or an early exit 1 with the reason (an uninstall that cannot locate
/// its own binary reverses nothing: there is no trustworthy binary path to
/// classify shims against, and guessing would risk the real toolchain).
#[allow(clippy::io_other_error)] // Keep the crate's Rust 1.70 MSRV.
pub fn cli(args: &[String]) -> i32 {
    let mut opts = Options::default();
    for flag in args {
        match flag.as_str() {
            "--dry-run" => opts.dry_run = true,
            "--keep-config" => opts.keep_config = true,
            "--help" | "-h" => {
                println!("usage: gantry uninstall [--dry-run] [--keep-config]");
                println!();
                println!("Removes the shim symlinks, the gantry binary, the state");
                println!("directory (run ledger, kill switch, crash bundles), the user");
                println!("config, and the systemd slice unit. Reports anything it could");
                println!("not remove; exit 1 when leftovers remain.");
                println!();
                println!("  --dry-run      Report what would be removed, touch nothing");
                println!("  --keep-config  Preserve ~/.config/gantry/config.toml");
                return 0;
            }
            other => {
                eprintln!("gantry uninstall: unknown flag '{other}'");
                return 2;
            }
        }
    }

    // The binary: current_exe, canonicalized (the same derivation
    // ensure_distinct_from_self trusts). Linux current_exe already resolves
    // symlinks; on platforms where it does not, a distinct symlinked path is
    // remembered as a wrapper so it does not dangle after the binary goes.
    let exe = std::env::current_exe().and_then(|p| {
        let canon = p
            .canonicalize()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok((canon, p))
    });
    let (self_binary, self_wrapper) = match exe {
        Ok((canon, raw)) => {
            let wrapper = if raw == canon { None } else { Some(raw) };
            (canon, wrapper)
        }
        Err(e) => {
            eprintln!("gantry uninstall: cannot locate its own binary: {e}");
            return 1;
        }
    };

    // State dirs: both spellings the writers use — the HOME-based one
    // (state.rs / runlog.rs) and the XDG-aware one (config.rs) — deduped.
    let mut state_dirs = Vec::new();
    if let Some(home) = std::env::var("HOME")
        .ok()
        .map(|h| Path::new(&h).join(".local").join("state").join("gantry"))
    {
        state_dirs.push(home);
    }
    if let Some(xdg) = dirs::state_dir().map(|d| d.join("gantry")) {
        if !state_dirs.contains(&xdg) {
            state_dirs.push(xdg);
        }
    }

    // User config dir (plan Component 3 layer 2) and the slice unit (plan
    // Component 6) — both reusing their owning modules' path rules.
    let config_dir = dirs::config_dir().map(|d| d.join("gantry"));
    let unit_path = crate::local::slice_unit_path();

    let targets = UninstallTargets {
        self_binary,
        self_wrapper,
        state_dirs,
        config_dir,
        unit_path,
    };

    let path_var = std::env::var("PATH").unwrap_or_default();
    let outcome = execute(&targets, &path_var, &opts);

    // The manager reload happens only on a real unit removal, and its
    // failure is a note, not a leftover: the unit file is gone either way,
    // and a stale manager view hurts nothing that a later reload (or
    // reboot) does not fix.
    if outcome.removed_unit.is_some() {
        if let Err(e) = crate::local::reload_user_manager() {
            eprintln!("[gantry] note: systemctl --user daemon-reload failed: {e}");
        }
    }

    print_report(&outcome);
    if outcome.is_clean() {
        0
    } else {
        1
    }
}

/// Render the report. `removed_*` lines read as past tense on a real run and
/// future tense on a dry run — the same list, so a dry run is exactly the
/// preview of the run.
fn print_report(outcome: &Outcome) {
    let verb = if outcome.dry_run {
        "would remove"
    } else {
        "removed"
    };
    for shim in &outcome.removed_shims {
        println!("{verb} shim {}", shim.display());
    }
    if let Some(binary) = &outcome.removed_binary {
        println!("{verb} binary {}", binary.display());
    }
    if let Some(wrapper) = &outcome.removed_wrapper {
        println!("{verb} {}", wrapper.display());
    }
    for dir in &outcome.removed_state_dirs {
        println!(
            "{verb} state directory {} (run ledger, kill switch, crash bundles)",
            dir.display()
        );
    }
    if let Some(config) = &outcome.removed_config {
        println!("{verb} config {}", config.display());
    }
    if let Some(unit) = &outcome.removed_unit {
        println!("{verb} systemd unit {}", unit.display());
    }
    if let Some(config) = &outcome.kept_config {
        println!("kept config {} (--keep-config)", config.display());
    }
    for note in &outcome.notes {
        println!("{note}");
    }
    for leftover in &outcome.leftovers {
        println!("{leftover}");
    }
    match &outcome.cargo_resolves_to {
        Some(cargo) => println!("cargo now resolves to: {}", cargo.display()),
        None => println!("no cargo found on PATH (the real toolchain is not on this shell's PATH)"),
    }
    if outcome.dry_run {
        println!("gantry uninstall: dry run — nothing was removed");
    } else if outcome.is_clean() {
        println!("gantry uninstall complete");
    } else {
        println!(
            "gantry uninstall incomplete — {} item(s) need manual attention",
            outcome.leftovers.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The fake gantry binary: a regular file with known bytes, named
    /// exactly `gantry` — the binary-name rule only deletes that name —
    /// inside a per-test temp dir, which is what keeps installs distinct.
    struct FakeInstall {
        // Held so the tree outlives the test body's assertions.
        _dir: tempfile::TempDir,
        binary: PathBuf,
    }

    impl FakeInstall {
        fn new(tag: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let binary = dir.path().join("gantry");
            fs::write(&binary, format!("gantry-binary-{tag}")).unwrap();
            FakeInstall { _dir: dir, binary }
        }

        fn targets(&self, state: &Path, config: &Path, unit: &Path) -> UninstallTargets {
            UninstallTargets {
                self_binary: self.binary.clone(),
                self_wrapper: None,
                state_dirs: vec![state.to_path_buf()],
                config_dir: Some(config.to_path_buf()),
                unit_path: Some(unit.to_path_buf()),
            }
        }
    }

    fn symlink(link: &Path, target: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn options(dry_run: bool, keep_config: bool) -> Options {
        Options {
            dry_run,
            keep_config,
        }
    }

    /// A throwaway install tree: state dir with ledger + kill switch, user
    /// config, slice unit — everything execute is allowed to remove.
    fn install_tree(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let state = root.join("state/gantry");
        let config = root.join("config/gantry");
        let unit = root.join("systemd/user").join("gantry.slice");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(state.join("runs.jsonl"), "{\"schema_version\":1}\n").unwrap();
        fs::write(state.join("state.toml"), "schema_version = 1\n").unwrap();
        fs::write(config.join("config.toml"), "[remote]\nbackend = \"none\"\n").unwrap();
        fs::write(&unit, "# Managed by gantry\n[Slice]\n").unwrap();
        (state, config, unit)
    }

    #[test]
    fn symlink_to_self_is_an_artifact() {
        let fake = FakeInstall::new("a");
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("cargo");
        symlink(&link, &fake.binary);
        assert!(is_gantry_artifact(&link, &fake.binary, &fake.binary));
    }

    #[test]
    fn relative_symlink_to_self_is_an_artifact() {
        // `ln -s gantry cargo` inside the binary's own directory — the
        // link text is relative and must still classify.
        let fake = FakeInstall::new("b");
        let dir = fake.binary.parent().unwrap();
        let link = dir.join("cargo");
        symlink(&link, Path::new(fake.binary.file_name().unwrap()));
        assert!(is_gantry_artifact(&link, &fake.binary, &fake.binary));
    }

    #[test]
    fn dangling_symlink_to_self_is_an_artifact() {
        // The binary-deleted-first state: the link text still names the
        // gantry path even though canonicalization now fails.
        let fake = FakeInstall::new("c");
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("cargo");
        symlink(&link, &fake.binary);
        fs::remove_file(&fake.binary).unwrap();
        assert!(is_gantry_artifact(&link, &fake.binary, &fake.binary));
    }

    #[test]
    fn copy_of_self_named_cargo_is_an_artifact() {
        let fake = FakeInstall::new("d");
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("cargo");
        fs::copy(&fake.binary, &copy).unwrap();
        assert!(is_gantry_artifact(&copy, &fake.binary, &fake.binary));
    }

    #[test]
    fn unrelated_cargo_is_never_an_artifact() {
        let fake = FakeInstall::new("e");
        let dir = tempfile::tempdir().unwrap();

        // A plain script named cargo.
        let real = script(dir.path(), "cargo", "#!/bin/sh\nexit 0\n");
        assert!(!is_gantry_artifact(&real, &fake.binary, &fake.binary));

        // A symlink onto someone else's toolchain (the rustup shape).
        let other = tempfile::tempdir().unwrap();
        let real_bin = script(other.path(), "real-cargo", "#!/bin/sh\nexit 0\n");
        let linked = dir.path().join("cargo.exe");
        symlink(&linked, &real_bin);
        assert!(!is_gantry_artifact(&linked, &fake.binary, &fake.binary));
    }

    #[test]
    fn scan_finds_shims_in_path_order_and_skips_dead_slots() {
        let fake = FakeInstall::new("g");
        let shim_a = tempfile::tempdir().unwrap();
        let shim_b = tempfile::tempdir().unwrap();
        symlink(&shim_a.path().join("cargo"), &fake.binary);
        symlink(&shim_b.path().join("cargo.exe"), &fake.binary);
        let path_var = format!(
            "/nonexistent-gantry-slot:{}:{}:/nonexistent-two",
            shim_a.path().display(),
            shim_b.path().display()
        );
        let found = scan_path(&path_var, &fake.binary, &fake.binary);
        assert_eq!(
            found,
            vec![shim_a.path().join("cargo"), shim_b.path().join("cargo.exe")]
        );
    }

    #[test]
    fn execute_removes_a_full_install_and_verifies_the_toolchain() {
        let fake = FakeInstall::new("h");
        let root = tempfile::tempdir().unwrap();
        let (state, config, unit) = install_tree(root.path());

        let shim_dir = tempfile::tempdir().unwrap();
        symlink(&shim_dir.path().join("cargo"), &fake.binary);
        let real_dir = tempfile::tempdir().unwrap();
        let real_cargo = script(real_dir.path(), "cargo", "#!/bin/sh\nexit 0\n");
        let path_var = format!(
            "{}:{}",
            shim_dir.path().display(),
            real_dir.path().display()
        );

        let targets = fake.targets(&state, &config, &unit);
        let out = execute(&targets, &path_var, &options(false, false));

        assert!(out.is_clean(), "leftovers: {:?}", out.leftovers);
        assert_eq!(out.removed_shims, vec![shim_dir.path().join("cargo")]);
        assert_eq!(out.removed_binary, Some(fake.binary.clone()));
        assert_eq!(out.removed_state_dirs, vec![state.clone()]);
        assert_eq!(out.removed_config, Some(config.join("config.toml")));
        assert_eq!(out.removed_unit, Some(unit.clone()));
        assert!(!shim_dir.path().join("cargo").exists(), "shim must be gone");
        assert!(!fake.binary.exists(), "binary must be gone");
        assert!(!state.exists(), "state dir must be gone");
        assert!(!config.join("config.toml").exists(), "config must be gone");
        assert!(!unit.exists(), "slice unit must be gone");
        // The real toolchain survives and is what cargo resolves to now —
        // the no-shadowing acceptance, in the positive.
        assert!(real_cargo.exists());
        assert_eq!(out.cargo_resolves_to, Some(real_cargo));
    }

    #[test]
    fn execute_is_idempotent_over_a_clean_box() {
        let fake = FakeInstall::new("i");
        let root = tempfile::tempdir().unwrap();
        let (state, config, unit) = install_tree(root.path());
        let real_dir = tempfile::tempdir().unwrap();
        let real_cargo = script(real_dir.path(), "cargo", "#!/bin/sh\nexit 0\n");
        let path_var = real_dir.path().display().to_string();

        let targets = fake.targets(&state, &config, &unit);
        let first = execute(&targets, &path_var, &options(false, false));
        assert!(first.is_clean(), "{:?}", first.leftovers);

        // Second run: nothing left to remove, still clean, still resolves.
        let second = execute(&targets, &path_var, &options(false, false));
        assert!(second.is_clean(), "{:?}", second.leftovers);
        assert!(second.removed_shims.is_empty());
        assert_eq!(second.removed_binary, None);
        assert_eq!(second.cargo_resolves_to, Some(real_cargo));
    }

    #[test]
    fn dry_run_classifies_but_touches_nothing() {
        let fake = FakeInstall::new("j");
        let root = tempfile::tempdir().unwrap();
        let (state, config, unit) = install_tree(root.path());
        let shim_dir = tempfile::tempdir().unwrap();
        symlink(&shim_dir.path().join("cargo"), &fake.binary);
        let path_var = shim_dir.path().display().to_string();

        let targets = fake.targets(&state, &config, &unit);
        let out = execute(&targets, &path_var, &options(true, false));

        assert!(out.is_clean(), "{:?}", out.leftovers);
        assert!(out.dry_run);
        assert_eq!(out.removed_shims, vec![shim_dir.path().join("cargo")]);
        assert_eq!(out.removed_binary, Some(fake.binary.clone()));
        // Nothing actually went anywhere.
        assert!(shim_dir.path().join("cargo").symlink_metadata().is_ok());
        assert!(fake.binary.is_file());
        assert!(state.is_dir());
        assert!(config.join("config.toml").is_file());
        assert!(unit.is_file());
    }

    #[test]
    fn keep_config_preserves_the_user_config() {
        let fake = FakeInstall::new("k");
        let root = tempfile::tempdir().unwrap();
        let (state, config, unit) = install_tree(root.path());

        let targets = fake.targets(&state, &config, &unit);
        let out = execute(&targets, "/nonexistent-gantry-slot", &options(false, true));

        assert!(out.is_clean(), "{:?}", out.leftovers);
        assert_eq!(out.kept_config, Some(config.join("config.toml")));
        assert_eq!(out.removed_config, None);
        assert!(config.join("config.toml").is_file());
        // The rest of the uninstall still happens — only the config is kept.
        assert!(!state.exists());
        assert!(!unit.exists());
    }

    #[test]
    fn foreign_named_binary_is_reported_not_deleted() {
        // A gantry copy masquerading as `cargo` as the running binary: the
        // rule-based deletion must refuse, and the report must say so. The
        // PATH scan skips the running file itself (the binary step owns it).
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("cargo");
        fs::write(&binary, "gantry-binary-under-wrong-name").unwrap();
        let root = tempfile::tempdir().unwrap();
        let (state, config, unit) = install_tree(root.path());
        let path_var = dir.path().display().to_string();

        let targets = UninstallTargets {
            self_binary: binary.clone(),
            self_wrapper: None,
            state_dirs: vec![state.clone()],
            config_dir: Some(config.clone()),
            unit_path: Some(unit.clone()),
        };
        let out = execute(&targets, &path_var, &options(false, false));

        assert!(!out.is_clean());
        assert!(
            out.leftovers.iter().any(|l| l.contains("gantry name")),
            "leftovers: {:?}",
            out.leftovers
        );
        assert_eq!(out.removed_binary, None);
        assert_eq!(out.removed_shims, Vec::<PathBuf>::new());
        assert!(binary.is_file(), "the misnamed binary must survive");
        // Everything else still went.
        assert!(!state.exists());
        assert!(!unit.exists());
    }

    #[test]
    fn content_identical_copy_on_path_is_removed() {
        // A copy install: gantry's bytes under the name `cargo` in a PATH
        // dir — the shadowing shape the acceptance cares about.
        let fake = FakeInstall::new("n");
        let shim_dir = tempfile::tempdir().unwrap();
        let copy = shim_dir.path().join("cargo");
        fs::copy(&fake.binary, &copy).unwrap();
        let real_dir = tempfile::tempdir().unwrap();
        let real_cargo = script(real_dir.path(), "cargo", "#!/bin/sh\nexit 0\n");
        let path_var = format!(
            "{}:{}",
            shim_dir.path().display(),
            real_dir.path().display()
        );

        let targets = UninstallTargets {
            self_binary: fake.binary.clone(),
            self_wrapper: None,
            state_dirs: vec![],
            config_dir: None,
            unit_path: None,
        };
        let out = execute(&targets, &path_var, &options(false, false));

        assert!(out.is_clean(), "{:?}", out.leftovers);
        assert_eq!(out.removed_shims, vec![copy.clone()]);
        assert!(!copy.exists(), "the copy must be gone");
        assert_eq!(out.cargo_resolves_to, Some(real_cargo));
    }

    #[test]
    fn removal_failure_lands_in_leftovers() {
        // A state dir whose parent is read-only: remove_dir_all clears the
        // children but cannot drop the dir itself, and the failure must be
        // reported per item, not abort the run.
        let fake = FakeInstall::new("l");
        let root = tempfile::tempdir().unwrap();
        let readonly_parent = root.path().join("ro");
        let blocked_state = readonly_parent.join("gantry");
        fs::create_dir_all(&blocked_state).unwrap();
        fs::write(blocked_state.join("x"), "y").unwrap();
        let mut perms = fs::metadata(&readonly_parent).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&readonly_parent, perms).unwrap();

        let targets = UninstallTargets {
            self_binary: fake.binary.clone(),
            self_wrapper: None,
            state_dirs: vec![blocked_state.clone()],
            config_dir: None,
            unit_path: None,
        };
        let out = execute(&targets, "/nonexistent-gantry-slot", &options(false, false));

        assert!(!out.is_clean());
        assert!(
            out.leftovers
                .iter()
                .any(|l| l.contains(&blocked_state.display().to_string())),
            "leftovers: {:?}",
            out.leftovers
        );
        assert!(blocked_state.is_dir(), "the blocked dir must survive");

        // Restore so the temp dir can be cleaned up on drop.
        let mut perms = fs::metadata(&readonly_parent).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&readonly_parent, perms).unwrap();
    }

    #[test]
    fn wrapper_symlink_is_removed_with_the_binary() {
        // A wrapper-symlink install: the running path is a symlink onto the
        // real binary elsewhere; both must go.
        let fake = FakeInstall::new("m");
        let bin_dir = tempfile::tempdir().unwrap();
        let wrapper = bin_dir.path().join("gantry");
        symlink(&wrapper, &fake.binary);

        let targets = UninstallTargets {
            self_binary: fake.binary.clone(),
            self_wrapper: Some(wrapper.clone()),
            state_dirs: vec![],
            config_dir: None,
            unit_path: None,
        };
        let out = execute(&targets, "/nonexistent-gantry-slot", &options(false, false));

        assert!(out.is_clean(), "{:?}", out.leftovers);
        assert_eq!(out.removed_binary, Some(fake.binary.clone()));
        assert_eq!(out.removed_wrapper, Some(wrapper.clone()));
        assert!(!fake.binary.exists());
        assert!(!wrapper.exists());
    }
}
