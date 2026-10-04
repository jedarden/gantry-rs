pub mod backend;
pub mod cap; // plan Component 6: the per-run cgroup cap (probe once per process, degrade loudly)
pub mod cli; // plan module layout: management-CLI diagnostics — why / explain / status, --json with schema_version
pub mod config;
pub mod crash; // plan Component 7: the crash flight recorder — redacted InfraFailure bundles + `gantry report`
pub mod decision;
pub mod doctor;
pub mod gate;
pub mod ledger; // plan Component 10: memoization, supersede-on-new-commit, flake flagging — one verdict-history module over runs.jsonl
pub mod local; // plan Component 6: LocalExecutor — scope, slice, fallback semaphore
pub mod quickcheck; // plan CLI surface: `gantry quickcheck` — shim, cap, git (Tier-0 proof)
pub mod refs;
pub mod runlog;
pub mod shim;
pub mod state;
#[cfg(test)]
pub(crate) mod testutil;
pub mod toolchain; // plan Component 5 parity preflight (pure half): org.gantry.toolchain label payload vs the local pin
pub mod uninstall; // plan §8 installer line: "`gantry uninstall` reverses it"
pub mod verdict;
