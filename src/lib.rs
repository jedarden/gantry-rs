pub mod backend;
pub mod cap; // plan Component 6: the per-run cgroup cap (probe once per process, degrade loudly)
pub mod config;
pub mod crash; // plan Component 7: flight recorder — REDACTED InfraFailure bundles + `gantry report`
pub mod decision;
pub mod doctor;
pub mod gate;
pub mod local; // plan Component 6: LocalExecutor — scope, slice, fallback semaphore
pub mod quickcheck; // plan CLI surface: `gantry quickcheck` — shim, cap, git (Tier-0 proof)
pub mod refs;
pub mod runlog;
pub mod shim;
pub mod state;
#[cfg(test)]
pub(crate) mod testutil;
pub mod verdict;
