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
// first piece (the self-recursion-guard basis), with resolve_real_binary
// (bf-614q), the re-exec guard (bf-1m69), and passthrough (bf-28dv) to follow.

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
// Held private by design (the bead scope says "private helper"): only the
// in-module resolve_real_binary (bf-614q) is meant to call it, and that
// consumer lands in the next child of the chain — so shim_dir has no caller at
// this commit. `#[allow(dead_code)]` keeps `-D warnings` green for the
// incremental build; remove it once bf-614q gives the helper a caller.
#[allow(dead_code)]
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
// shim_dir): only the in-module resolve_real_binary (bf-614q) is meant to call
// it, and that consumer lands in the next child of the chain — so
// strip_dir_from_path has no caller at this commit. `#[allow(dead_code)]`
// keeps `-D warnings` green for the incremental build; remove it once bf-614q
// gives the helper a caller.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
