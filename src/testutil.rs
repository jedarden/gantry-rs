//! Test-only helpers shared across the crate's unit tests.
//!
//! The test binary runs every `#[cfg(test)]` module in one process under the
//! default parallel harness, so anything process-wide (the current directory,
//! environment variables) is shared mutable state. Gantry's hermeticity rule
//! is that tests never touch it: repo operations take explicit paths
//! (`check_git_gate_in`, `RefPusher::push_in`, `Config::repo_config_path_in`),
//! and this module provides the tripwire that keeps it that way.

/// The process working directory as it was when the first test looked.
///
/// Tests assert the cwd still equals this snapshot after running, which fails
/// if anyone reintroduces a `set_current_dir` — the pattern that used to make
/// parallel runs cascade (one test's saved "original" directory being another
/// test's already-deleted TempDir). One snapshot for the whole binary, so the
/// comparison is meaningful no matter which module's tests run first.
pub(crate) fn cwd_at_test_start() -> std::path::PathBuf {
    static CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    CWD.get_or_init(|| std::env::current_dir().expect("current_dir"))
        .clone()
}
