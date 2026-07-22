// gantry — the shim's pure dispatch helpers.
//
// Phase 0.5 walking skeleton (plan §"Phase 0.5", Components §1 "Shim &
// dispatcher"). This file holds the no-I/O, decision-only pieces of the shim
// that main.rs consults *before* any pipeline stage runs. It is deliberately a
// self-contained leaf: it touches no filesystem, spawns no process, and
// references no other gantry module (gate/refs/backend/local/runlog), so it
// compiles and unit-tests standalone ahead of the rest of the skeleton wiring
// (bf-2w7).
//
// `invocation_name` lives here because argv[0] is the dispatch key main.rs
// matches on ('cargo' vs 'gantry' vs unknown). Real-binary resolution and the
// passthrough exec join it here across the bf-614q chain: `shim_dir` is the
// first piece (the self-recursion-guard basis), and resolve_real_binary
// (bf-614q) composes it with the PATH lookup; the re-exec guard (bf-1m69) and
// passthrough (bf-28dv) follow.

/// The sentinel dispatch key returned when argv[0] is absent or basename-less.
///
/// Chosen so a degenerate invocation routes to the management CLI
/// (`gantry version` / `gantry help`) rather than silently treating a broken
/// argv[0] as a cargo offload, and so the fallback is a constant main.rs can
/// match against without parsing.
const SENTINEL: &str = "gantry";

/// The dispatch key derived from `argv[0]`: the basename of how the binary was
/// invoked, with any directory component stripped.
///
/// main.rs matches the result to pick a profile — `cargo` selects the cargo
/// tool profile (which may offload an intercepted subcommand), `gantry` selects
/// the management CLI, and anything else falls through to the cargo profile as
/// the safe default (plan §1).
///
/// `/home/u/.local/bin/cargo` → `cargo`; `cargo` → `cargo`; `gantry` → `gantry`.
///
/// Never panics. An empty `argv`, an `argv[0]` with no basename (e.g. `""`,
/// `"."`, `".."`, a bare `"/"`), or a path that ends in a separator all yield
/// the stable sentinel [`SENTINEL`] — dispatch then lands on the management CLI
/// instead of indexing into a hole.
pub fn invocation_name(argv: &[String]) -> String {
    // An absent argv[0] (empty argv) carries no invocation name; route to the
    // management CLI deterministically rather than indexing into a hole.
    let Some(argv0) = argv.first() else {
        return SENTINEL.to_string();
    };

    // `Path::file_name` is the basename of argv[0] with every directory
    // component stripped, in pure path logic (no filesystem access). It returns
    // `None` for the empty string, `.` and `..`, and a lone root — exactly the
    // shapes that carry no usable dispatch key. `to_str` never fails here
    // because argv0 is already a UTF-8 `String`, but is kept for type hygiene.
    match std::path::Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
    {
        Some(name) if !name.is_empty() => name.to_string(),
        // basename-less argv[0] (including a trailing-slash-only path): fall to
        // the sentinel so dispatch stays deterministic and panic-free.
        _ => SENTINEL.to_string(),
    }
}

/// The directory gantry must strip from PATH before resolving the real cargo —
/// namely the directory that holds the gantry binary itself.
///
/// Returns the *parent* of `std::env::current_exe()`: a PATH entry pointing
/// here would re-resolve `cargo` to the gantry shim, so this strip is the
/// self-recursion guard's basis (plan §1 "Real-binary resolution"). Pure path
/// logic — no PATH split, no cargo lookup, no filesystem stat beyond
/// `current_exe()`, and no reference to Config; the future resolve_real_binary
/// (bf-614q) consumes this to build the stripped search path.
///
/// Never panics. `current_exe()` can fail (platform path-resolution limits, a
/// binary deleted mid-run, `/proc` unavailable on Linux); the failure surfaces
/// as a human-readable `Err` rather than a panic or `unwrap`. On success the
/// parent directory is returned — a directory path, never the binary file.
//
// Held private by design (the bead scope says "private helper"): only
// resolve_real_binary is meant to call it.
fn shim_dir() -> Result<std::path::PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("gantry cannot locate its own binary: {e}"))?;
    // `parent()` is None only for a path with no directory component (a bare
    // filename). `current_exe()` always carries a directory on supported
    // platforms, so this is a defensive fallback surfaced as an error rather
    // than an unwrap that could panic on an exotic shape.
    exe.parent().map(std::path::PathBuf::from).ok_or_else(|| {
        format!(
            "gantry binary path has no parent directory: {}",
            exe.display()
        )
    })
}

/// The character that separates entries in a PATH-style environment variable.
///
/// `:` on Unix-family targets, `;` on Windows. There is no stable `std`
/// constant for the *PATH-list* separator — `std::path::MAIN_SEPARATOR` is the
/// path-component separator (`/` or `\`), a different thing — so the platform
/// split is made explicit here. Pure constant logic; no environment access.
#[cfg(unix)]
const PATH_SEP: &str = ":";
#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(any(unix, windows)))]
compile_error!("gantry targets Unix and Windows; define a PATH separator for this platform");

/// Strip a target directory from a PATH-style string.
///
/// Splits `path_var` on the OS PATH separator ([`PATH_SEP`]) and drops every
/// entry whose [`std::path::Path`] equals `dir`, preserving the order of the
/// survivors, then re-joins them with [`PATH_SEP`]. Pure string/path logic —
/// no environment reads, no filesystem canonicalization, no `current_exe`.
/// `dir` is a parameter (rather than derived from the process) so this is
/// independently unit-testable with crafted PATH literals.
///
/// This is the self-recursion guard's filter: the future resolve_real_binary
/// (bf-614q) strips the shim dir ([`shim_dir`]) from PATH so the real-cargo
/// lookup cannot re-resolve `cargo` to the gantry shim itself (plan §1
/// "Real-binary resolution"). `dir` is compared as a [`std::path::Path`], so
/// trailing-slash and `.`-component variants of the same directory match.
///
/// Separator hygiene: the filter only *removes* entries, so re-joining the
/// survivors can never introduce a spurious empty (CWD) entry — no leading,
/// double, or trailing separator appears that was not already implied by the
/// input. Existing empty entries already present in `path_var` are preserved
/// (the "dir absent → effectively unchanged" guarantee): an empty entry is not
/// `dir`, so it survives untouched.
//
// Held private by design (a resolve_real_binary-internal helper, like
// shim_dir): only resolve_real_binary is meant to call it.
fn strip_dir_from_path(path_var: &str, dir: &std::path::Path) -> String {
    // `split` on a single separator yields one substring per PATH entry, with
    // empty entries (leading/double/trailing separators) preserved as "" —
    // which re-join back to the original separator shape. Comparing each entry
    // as a Path (rather than a raw string) makes `/b` match a `/b/` or
    // `/x/./b`-style entry without canonicalizing anything on disk.
    path_var
        .split(PATH_SEP)
        .filter(|entry| std::path::Path::new(*entry) != dir)
        .collect::<Vec<&str>>()
        .join(PATH_SEP)
}

/// Find an executable named `name` by walking `path` left to right.
///
/// Splits `path` on the OS PATH separator ([`PATH_SEP`]) and, for each entry in
/// order, checks whether `<entry>/<name>` is an existing executable regular
/// file. Returns [`Ok`] with the first matching path — PATH order respected —
/// or [`Err`] naming `name` if no entry matches. A pure lookup over a
/// caller-supplied PATH string: no environment read of its own and no
/// `current_exe`. The caller is responsible for stripping gantry's own shim dir
/// ([`shim_dir`] via [`strip_dir_from_path`]) first, so the lookup cannot
/// re-resolve `cargo` to the gantry shim itself (plan §1 "Real-binary
/// resolution").
///
/// Non-existent entries, directories, and non-executable or non-regular files
/// are skipped silently: a stale or misconfigured PATH slot is a skip, not a
/// failure. Only the total absence of a match is an error, and the message
/// always names `name` so a missing-real-cargo diagnostic is actionable — this
/// helper never silently falls back to a default binary.
///
/// Never panics. A filesystem error on a given candidate (the directory gone,
/// a permission-denied stat) surfaces as "not a match" for that entry and the
/// walk continues, because one bad PATH slot must not abort the whole lookup.
//
// Held private by design (a resolve_real_binary-internal helper, like
// shim_dir and strip_dir_from_path): only resolve_real_binary is meant to
// call it.
fn find_in_path(name: &str, path: &str) -> Result<std::path::PathBuf, String> {
    for entry in path.split(PATH_SEP) {
        let candidate = std::path::Path::new(entry).join(name);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "gantry could not find `{name}` on PATH; install the real binary or set `real_binary` in config"
    ))
}

/// Whether `candidate` is an existing executable regular file.
///
/// `std::fs::metadata` (not `symlink_metadata`) follows a symlink to its
/// target, so a PATH entry pointing at a symlinked binary resolves correctly;
/// a broken symlink, a missing file, a directory, or any non-regular entry is a
/// non-match rather than an error — a stat failure is a skip for [`find_in_path`].
/// The platform-specific notion of "executable" is delegated to
/// [`is_executable`].
fn is_executable_file(candidate: &std::path::Path) -> bool {
    let Ok(md) = std::fs::metadata(candidate) else {
        return false;
    };
    md.is_file() && is_executable(candidate, &md)
}

/// Whether a regular file is executable.
///
/// Platform-specific because the notion differs: Unix consults the mode bits
/// carried in [`std::fs::Metadata`]; Windows carries no execute bit and infers
/// runnability from the file extension. Reads no environment (no `PATHEXT`) —
/// the conventional executable extensions are fixed here so this helper honors
/// the "no env reads of its own" contract shared with [`find_in_path`].
#[cfg(unix)]
fn is_executable(_candidate: &std::path::Path, md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Any execute bit (user/group/other) qualifies — matching shell PATH
    // lookup, which treats a file runnable by anyone as found.
    md.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(candidate: &std::path::Path, _md: &std::fs::Metadata) -> bool {
    // No execute bit exists on Windows; runnability is conventionally a
    // property of the extension. A PATHEXT env read would violate the helper's
    // "no env reads of its own" contract, so the common executable extensions
    // are matched directly.
    matches!(
        candidate.extension().and_then(|e| e.to_str()),
        Some("exe") | Some("bat") | Some("cmd") | Some("com")
    )
}

/// Resolve the real `cargo` binary the shim should hand off to (plan §1
/// "Real-binary resolution", Components §1 "Shim & dispatcher").
///
/// Composes the three private helpers into the top-level resolution path the
/// dispatcher consults before any pipeline stage runs:
///
/// 1. **Override short-circuit** — when
///    [`crate::config::Config::real_binary`] is `Some`, return that path
///    verbatim with *no* PATH search. The explicit override is trusted as-is
///    (it need not exist on disk; exec-check is a later stage's concern), so a
///    pinned toolchain is honored without touching the environment.
/// 2. **PATH lookup** — otherwise, derive gantry's own binary dir ([`shim_dir`]),
///    strip it from `PATH` ([`strip_dir_from_path`]) so the walk cannot
///    re-resolve `cargo` to the gantry shim itself (the self-recursion guard,
///    enforced here at the resolution layer), then walk the survivors
///    left-to-right for `cargo` ([`find_in_path`]).
///
/// Never silently falls through to the gantry binary or to `None`: a missing
/// `cargo` on the stripped PATH propagates [`find_in_path`]'s `Err` verbatim, so
/// a broken install surfaces as a diagnostic instead of an infinite re-exec.
/// Never panics — every fallible step maps to a human-readable `Err`. Spawns no
/// process and consults no other gantry pipeline module (gate/refs/backend/
/// local/runlog): this is decision-only resolution.
///
/// This is the integration seam the bf-614q chain lands. [`shim_dir`],
/// [`strip_dir_from_path`], and [`find_in_path`] each shipped standalone in
/// earlier children; `resolve_real_binary` is their first and only caller, which
/// is why their provisional `#[allow(dead_code)]` attributes retired at this
/// commit.
pub fn resolve_real_binary(cfg: &crate::config::Config) -> Result<std::path::PathBuf, String> {
    // 1. Explicit override: trust it verbatim and skip the PATH search entirely.
    if let Some(path) = &cfg.real_binary {
        return Ok(path.clone());
    }

    // 2. Exclude gantry's own binary dir so the lookup cannot re-resolve
    //    `cargo` to the gantry shim (self-recursion guard), then walk the
    //    surviving PATH entries left-to-right for `cargo`.
    let shim = shim_dir()?;
    let path_var = std::env::var("PATH")
        .map_err(|e| format!("gantry cannot read the PATH environment variable: {e}"))?;
    let stripped = strip_dir_from_path(&path_var, &shim);
    find_in_path("cargo", &stripped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Mutex;

    #[test]
    fn strips_directory_to_basename() {
        // The load-bearing case: the shim sits ahead of the real toolchain
        // under a full path, but dispatch keys on the bare name.
        assert_eq!(
            invocation_name(&["/home/u/.local/bin/cargo".to_string()]),
            "cargo"
        );
    }

    #[test]
    fn bare_names_pass_through_unchanged() {
        assert_eq!(invocation_name(&["cargo".to_string()]), "cargo");
        assert_eq!(invocation_name(&["gantry".to_string()]), "gantry");
    }

    #[test]
    fn relative_path_basename_is_taken() {
        assert_eq!(invocation_name(&["./cargo".to_string()]), "cargo");
        assert_eq!(invocation_name(&["bin/cargo".to_string()]), "cargo");
    }

    #[test]
    fn unknown_name_is_returned_verbatim() {
        // main.rs's `other` arm matches whatever falls through; the value must
        // survive untouched so the "unrecognized invocation" diagnostic is exact.
        assert_eq!(invocation_name(&["frobnicate".to_string()]), "frobnicate");
    }

    #[test]
    fn empty_argv_does_not_panic() {
        // No argv[0] at all — must yield the sentinel, never index-panic.
        assert_eq!(invocation_name(&[]), "gantry");
    }

    #[test]
    fn empty_argv0_falls_to_sentinel() {
        assert_eq!(invocation_name(&["".to_string()]), "gantry");
    }

    #[test]
    fn no_basename_falls_to_sentinel() {
        // ".", "..", and a bare root carry no dispatchable name.
        assert_eq!(invocation_name(&[".".to_string()]), "gantry");
        assert_eq!(invocation_name(&["..".to_string()]), "gantry");
        assert_eq!(invocation_name(&["/".to_string()]), "gantry");
    }

    #[test]
    fn trailing_separator_does_not_panic() {
        // A directory-like argv[0] must not crash. The basename is the last
        // named component; only a separator-only path collapses to the sentinel.
        assert_eq!(invocation_name(&["/usr/bin/cargo/".to_string()]), "cargo");
        assert_eq!(invocation_name(&["///".to_string()]), "gantry");
    }

    #[test]
    fn ignores_argv_beyond_the_first() {
        // Dispatch keys on argv[0] only; trailing args are none of its concern.
        assert_eq!(
            invocation_name(&["cargo".to_string(), "test".to_string()]),
            "cargo"
        );
    }

    #[test]
    fn sentinel_is_stable_and_matchable() {
        // main.rs matches the return against &'static str; the sentinel is
        // constant across every degenerate input so dispatch is deterministic.
        assert_eq!(invocation_name(&[]), invocation_name(&["".to_string()]));
        assert_eq!(invocation_name(&[".".to_string()]), SENTINEL);
    }

    #[test]
    fn shim_dir_returns_the_parent_not_the_binary() {
        // The self-recursion-guard basis: shim_dir is the directory that holds
        // the gantry binary, never the binary itself — a PATH entry pointing
        // here is exactly what must be stripped before resolving real cargo.
        let dir = shim_dir().expect("current_exe resolves under cargo test");
        let exe = std::env::current_exe().expect("current_exe resolves under cargo test");
        assert_ne!(dir, exe, "shim_dir must be the parent, not the binary");
        assert_eq!(
            dir,
            exe.parent().expect("the test binary has a parent dir"),
            "shim_dir must be exactly the parent of current_exe",
        );
    }

    #[test]
    fn shim_dir_has_a_nonempty_name() {
        // Smoke: the returned directory carries a real, non-empty final
        // component (the dir name a PATH entry would match against).
        let dir = shim_dir().expect("current_exe resolves under cargo test");
        let name = dir
            .file_name()
            .expect("shim_dir has a file_name under cargo test");
        assert!(!name.is_empty(), "shim_dir file_name must be non-empty");
    }

    #[test]
    fn strip_removes_a_middle_entry_preserving_order() {
        // The load-bearing case: the shim dir sits among real toolchain dirs,
        // and the survivors keep their relative order so resolve_real_binary
        // searches them in the user's intended sequence.
        let path = ["/a", "/b", "/c"].join(PATH_SEP);
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            ["/a", "/c"].join(PATH_SEP),
        );
    }

    #[test]
    fn strip_absent_dir_returns_path_unchanged() {
        // A dir not on PATH must round-trip exactly — no reordering, no
        // canonicalization, no separator rewrites.
        let path = ["/a", "/b", "/c"].join(PATH_SEP);
        assert_eq!(strip_dir_from_path(&path, std::path::Path::new("/x")), path,);
    }

    #[test]
    fn strip_removes_every_occurrence_of_a_duplicated_dir() {
        // A misconfigured PATH may list the shim dir twice; both copies must
        // go, not just the first.
        let path = ["/a", "/b", "/b", "/c"].join(PATH_SEP);
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            ["/a", "/c"].join(PATH_SEP),
        );
    }

    #[test]
    fn strip_leading_dir_introduces_no_leading_separator() {
        // Stripping the first entry must not leave a stray leading separator,
        // which a downstream lookup would read as a leading CWD entry.
        let path = ["/b", "/a", "/c"].join(PATH_SEP);
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            ["/a", "/c"].join(PATH_SEP),
        );
    }

    #[test]
    fn strip_trailing_dir_introduces_no_trailing_separator() {
        // Stripping the last entry must not leave a stray trailing separator.
        let path = ["/a", "/c", "/b"].join(PATH_SEP);
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            ["/a", "/c"].join(PATH_SEP),
        );
    }

    #[test]
    fn strip_sole_matching_entry_yields_empty_not_a_separator() {
        // Removing the only entry must yield an empty string, never a lone
        // separator (which would decode to a single CWD entry).
        let path = String::from("/b");
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            String::new(),
        );
    }

    #[test]
    fn strip_sole_nonmatching_entry_survives() {
        let path = String::from("/a");
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            String::from("/a"),
        );
    }

    #[test]
    fn strip_empty_path_round_trips_to_empty() {
        // No entries to filter — and no spurious entry manufactured either.
        assert_eq!(
            strip_dir_from_path("", std::path::Path::new("/b")),
            String::new(),
        );
    }

    #[test]
    fn strip_matches_a_trailing_slash_variant_of_the_dir() {
        // Path equality (not string equality) drives the match: `/b/` is the
        // same directory as `/b`, so a slash-padded PATH entry is stripped too.
        let path = ["/a", "/b/", "/c"].join(PATH_SEP);
        assert_eq!(
            strip_dir_from_path(&path, std::path::Path::new("/b")),
            ["/a", "/c"].join(PATH_SEP),
        );
    }

    #[test]
    fn strip_preserves_an_unrelated_empty_cwd_entry() {
        // An existing empty entry (a `::` CWD slot the user set) is not `dir`,
        // so it is preserved verbatim — the filter never *introduces* one, and
        // never silently drops the user's intended CWD either.
        let path = ["/a", "", "/c"].join(PATH_SEP);
        assert_eq!(strip_dir_from_path(&path, std::path::Path::new("/b")), path,);
    }

    // --- find_in_path fixtures ----------------------------------------------
    //
    // The executable-bit semantics under test are Unix-specific, so these
    // fixtures and the find_in_path tests are `#[cfg(unix)]`. shim.rs stays
    // std-only (no `tempfile` dev-dependency): TempDir is a minimal stand-in,
    // a uniquely-named subdir of std::env::temp_dir() removed wholesale on drop.
    // The dir name mixes the test-binary PID with a per-test suffix so parallel
    // tests in one process never collide.

    /// A throwaway directory under the system temp, removed on drop.
    #[cfg(unix)]
    struct TempDir(std::path::PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new(suffix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gantry-find_in_path-{}-{suffix}",
                std::process::id(),
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            // Best-effort: a failed cleanup (the dir already gone) must not
            // fail the test or panic on drop.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write `<dir>/<name>` as a regular file with the executable bit set.
    #[cfg(unix)]
    fn touch_executable(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
        path
    }

    /// Write `<dir>/<name>` as a regular file with no executable bit.
    #[cfg(unix)]
    fn touch_plain(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, b"not executable\n").expect("write fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod fixture");
        path
    }

    #[cfg(unix)]
    fn path_string(dirs: &[&std::path::Path]) -> String {
        dirs.iter()
            .map(|d| d.to_str().expect("temp path is utf-8"))
            .collect::<Vec<_>>()
            .join(PATH_SEP)
    }

    #[cfg(unix)]
    #[test]
    fn find_in_path_returns_the_executable_in_a_dir() {
        // The load-bearing case: the stripped PATH points at the dir holding
        // the real cargo, and that file is an executable — the lookup returns
        // exactly that path.
        let dir = TempDir::new("found");
        let cargo = touch_executable(dir.path(), "cargo");
        let path = path_string(&[dir.path()]);
        assert_eq!(find_in_path("cargo", &path), Ok(cargo));
    }

    #[cfg(unix)]
    #[test]
    fn find_in_path_errs_naming_the_binary_when_absent() {
        // A PATH with no matching entry must Err — and the message must name
        // the binary so a missing-real-cargo diagnostic stays actionable.
        let dir = TempDir::new("absent");
        let path = path_string(&[dir.path()]);
        let err = find_in_path("cargo", &path).expect_err("no cargo in dir");
        assert!(err.contains("cargo"), "error must name the binary: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn find_in_path_respects_path_order() {
        // The first matching entry wins: two dirs each hold an executable
        // cargo, and the leftmost (earlier in PATH) is returned.
        let dir_a = TempDir::new("order-a");
        let dir_b = TempDir::new("order-b");
        let cargo_a = touch_executable(dir_a.path(), "cargo");
        let _cargo_b = touch_executable(dir_b.path(), "cargo");
        let path = path_string(&[dir_a.path(), dir_b.path()]);
        assert_eq!(find_in_path("cargo", &path), Ok(cargo_a));
    }

    #[cfg(unix)]
    #[test]
    fn find_in_path_skips_stale_nonexecutable_and_directory_entries() {
        // A stale (now-missing) dir, a non-executable file, and a directory
        // named like the binary are all skipped without erroring; the real
        // executable in a later dir is what the lookup lands on.
        let stale = TempDir::new("stale");
        let plain = TempDir::new("plain");
        let _ = touch_plain(plain.path(), "cargo"); // non-executable → skipped
        let dir_named_cargo = TempDir::new("dircargo");
        std::fs::create_dir(dir_named_cargo.path().join("cargo")).expect("mkdir fixture");
        let real = TempDir::new("real");
        let cargo = touch_executable(real.path(), "cargo");

        // Remove `stale` so its PATH entry resolves to nothing on disk — a
        // skip, not a failure. The stored path string is still valid afterwards.
        std::fs::remove_dir_all(stale.path()).expect("remove stale dir");

        let path = path_string(&[
            stale.path(),
            plain.path(),
            dir_named_cargo.path(),
            real.path(),
        ]);
        assert_eq!(find_in_path("cargo", &path), Ok(cargo));
    }

    // --- resolve_real_binary: PATH-mutating integration tests ----------------
    //
    // resolve_real_binary reads the process-global PATH, and cargo runs unit
    // tests across parallel threads that all share that one environment. The
    // harness below serializes every PATH mutation behind ENV_LOCK and restores
    // the original value on drop (Drop runs even if the closure panics, while
    // the lock is still held), so the override short-circuit test can run
    // against a controlled empty PATH without leaking that mutation to a
    // sibling test.

    /// Serializes every test that mutates the process-global PATH.
    ///
    /// Tests run in parallel threads sharing one environment; holding this lock
    /// for the whole `with_path` body guarantees only one test touches PATH at a
    /// time, and the restore-on-drop completes before the next waiter acquires
    /// it. Recovered from poison on acquire so a panicking prior test does not
    /// wedge the rest of the suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that restores PATH to its saved value on drop.
    ///
    /// `None` means PATH was unset before the test ran, so drop `remove_var`s
    /// it rather than reinstating an empty string — the original absent state is
    /// what must survive. Drop runs even on panic (the whole point: a failing
    /// assertion must not leak a mutated PATH into a sibling test).
    struct RestorePath(Option<std::ffi::OsString>);

    impl Drop for RestorePath {
        fn drop(&mut self) {
            match &self.0 {
                Some(saved) => std::env::set_var("PATH", saved),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Run `f` with PATH temporarily set to `new_path`, restoring it after.
    ///
    /// Acquires `ENV_LOCK` for the duration (poison-recovered by unwrapping the
    /// `PoisonError` via `into_inner`, so a panicking prior test does not wedge
    /// the suite), snapshots the original PATH (present-or-absent), installs
    /// `new_path`, runs `f`, and restores the snapshot when `RestorePath` drops
    /// — including on panic, the load-bearing guarantee: a test that
    /// asserts-then-panics still hands back a clean environment. The lock is
    /// released only after the restore (declared first, dropped last), so the
    /// mutation is never visible to a concurrently-running waiter.
    fn with_path<R>(new_path: &str, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _restore = RestorePath(std::env::var_os("PATH"));
        std::env::set_var("PATH", new_path);
        f()
        // `_restore` drops here, then `_guard`: PATH is reinstated while the
        // lock is still held, including when `f` panicked. The leading
        // underscore silences the "unused binding" lint — the value is never
        // read, only its Drop — while still living to end-of-scope (unlike a
        // bare `_`, which would drop it immediately and defeat the guard).
    }

    #[test]
    fn resolve_real_binary_returns_override_verbatim_with_empty_path() {
        // The override short-circuit (plan §1): an explicit real_binary is
        // trusted as-is and returned verbatim, with no PATH search at all. Run
        // under an empty PATH to prove the override wins independent of the
        // environment — a pinned toolchain is honored without consulting PATH.
        let mut cfg = Config::hardcoded();
        cfg.real_binary = Some(std::path::PathBuf::from("/usr/bin/cargo"));
        let resolved = with_path("", || resolve_real_binary(&cfg));
        assert_eq!(
            resolved,
            Ok(std::path::PathBuf::from("/usr/bin/cargo")),
            "explicit override must win verbatim even with an empty PATH",
        );
    }

    #[test]
    fn resolve_real_binary_self_recursion_guard_yields_err_when_path_is_only_shim_dir() {
        // The self-recursion guard (plan §1): with PATH reduced to ONLY gantry's
        // own shim dir and no override (real_binary == None), resolve_real_binary
        // strips that dir (it excludes current_exe's dir) and the search path
        // collapses to nothing, so the cargo walk finds no binary and the resolver
        // returns Err — it never re-resolves `cargo` to the gantry binary itself.
        let cfg = Config::hardcoded(); // real_binary == None → the PATH-lookup branch
        let shim_dir = shim_dir().expect("current_exe resolves under cargo test");
        let shim_dir_str = shim_dir
            .to_str()
            .expect("shim_dir is a utf-8 path under cargo test");
        let err = with_path(shim_dir_str, || resolve_real_binary(&cfg)).expect_err(
            "a PATH containing only the shim dir must yield Err, never the gantry binary",
        );
        assert!(
            err.contains("cargo"),
            "error must name the binary so the diagnostic stays actionable: {err}",
        );
    }

    #[test]
    fn resolve_real_binary_finds_real_cargo_on_inherited_path() {
        // The happy path (plan §1 "Real-binary resolution"): with no override
        // (real_binary == None) and the inherited real PATH, the resolver walks
        // the shim-stripped PATH and returns the genuine cargo. ENV_LOCK is held
        // for the read even though PATH is not mutated here, so no sibling
        // PATH-mutating test (the override short-circuit or the self-recursion-
        // guard case) can be mid-mutation when this test samples the inherited
        // PATH — without the lock, a concurrent `with_path` could momentarily
        // blank PATH and make this resolution spuriously fail.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cfg = Config::hardcoded(); // real_binary == None → the PATH-lookup branch
        let resolved = resolve_real_binary(&cfg)
            .expect("the inherited PATH must resolve to a real cargo under cargo test");
        let name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .expect("the resolved cargo path carries a file_name");
        let expected = if cfg!(windows) { "cargo.exe" } else { "cargo" };
        assert_eq!(
            name, expected,
            "resolved binary basename must be the cargo executable for the platform",
        );
    }
}
