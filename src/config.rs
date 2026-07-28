// gantry — hardcoded configuration for the Phase 0.5 walking skeleton.
//
// This is the pure-data foundation for the skeleton (plan §"Phase 0.5"): a
// literal struct with no config-file layering, no last-known-good snapshot, and
// no trust boundary — all of that lands in Phase 1a (plan Components,
// "src/config.rs"). The skeleton receives one fixed [`Config`] from
// [`Config::hardcoded`] and asks it two questions: does this subcommand get
// intercepted, and how long may a remote run take. Nothing here does I/O,
// process spawning, or references any other gantry module.

use std::path::PathBuf;
use std::time::Duration;

/// Fixed configuration for the walking skeleton.
///
/// Every field is a hardcoded constant; there is no file parsing or layering.
/// The skeleton resolves the real cargo via PATH unless `real_binary`
/// overrides it, offloads only the subcommands in `intercept_list`, and caps a
/// remote run at the configured `deadline`.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Phase 0.5 skeleton: fields reserved for Phase 1a
pub struct Config {
    /// Optional override for the real cargo binary; `None` means "resolve via
    /// PATH" (plan Components §1, "Shim & dispatcher").
    pub real_binary: Option<PathBuf>,
    /// The git remote to push epoch refs to (plan §4 RefPusher).
    pub ci_remote: String,
    /// Subcommands eligible for remote offload, matched exactly against argv[1]
    /// (EC-07: cargo aliases expanding to `test` are not chased).
    pub intercept_list: Vec<String>,
    /// Wall-clock budget for a single remote run before it is treated as an
    /// InfraFailure (plan failure modes: client deadline exceeded).
    deadline: Duration,
}

impl Config {
    /// The one configuration the walking skeleton ships with: intercept only
    /// `cargo test`, push to `origin`, resolve the real binary via PATH, and
    /// allow a run forty minutes before declaring an infra timeout.
    pub fn hardcoded() -> Self {
        Config {
            real_binary: None,
            ci_remote: "origin".to_string(),
            intercept_list: vec!["test".to_string()],
            deadline: Duration::from_secs(40 * 60),
        }
    }

    /// True iff `subcommand` is in the intercepted set (default: `test`).
    ///
    /// Matches argv[1] exactly; `build`, `version`, and anything else
    /// passthrough to the real cargo untouched (the fast path, plan DD-1).
    pub fn intercepts(&self, subcommand: &str) -> bool {
        self.intercept_list.iter().any(|s| s.as_str() == subcommand)
    }

    /// The remote-run deadline — a positive [`Duration`].
    #[allow(dead_code)] // Phase 0.5 skeleton: reserved for Phase 1a
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercepts_only_test_by_default() {
        let cfg = Config::hardcoded();
        // The skeleton offloads exactly `cargo test`.
        assert!(cfg.intercepts("test"));
        // Non-intercepted subcommands passthrough to the real cargo.
        assert!(!cfg.intercepts("build"));
        assert!(!cfg.intercepts("version"));
        assert!(!cfg.intercepts("check"));
        assert!(!cfg.intercepts("run"));
        // An empty / absent argv[1] is never intercepted.
        assert!(!cfg.intercepts(""));
    }

    #[test]
    fn deadline_is_positive() {
        let cfg = Config::hardcoded();
        assert!(cfg.deadline() > Duration::ZERO);
    }

    #[test]
    fn hardcoded_ci_remote_is_usable() {
        // A real placeholder, not the empty string — the skeleton hands GitGate
        // a remote name it can actually look up.
        let cfg = Config::hardcoded();
        assert!(!cfg.ci_remote.is_empty());
    }

    #[test]
    fn real_binary_defaults_to_path_resolution() {
        // None => resolve the real cargo via PATH (plan §1). An override is a
        // later-config concern; the skeleton never sets one.
        let cfg = Config::hardcoded();
        assert!(cfg.real_binary.is_none());
    }

    #[test]
    fn config_is_clone_for_backend_construction() {
        // main.rs hands a clone to the backend while keeping its own reference,
        // so Config must be Clone without disturbing the original.
        let cfg = Config::hardcoded();
        let owned = cfg.clone();
        assert!(owned.intercepts("test"));
        assert!(cfg.intercepts("test"));
    }
}
