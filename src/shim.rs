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
// passthrough exec join it here in later stages of the bf-6d4 split (bf-614q,
// bf-1m69, bf-28dv); this stage adds only the name extraction.

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
}
