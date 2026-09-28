pub mod backend;
pub mod config;
pub mod decision;
pub mod doctor;
pub mod gate;
pub mod local; // plan Component 6: LocalExecutor — scope, slice, fallback semaphore
pub mod refs;
pub mod runlog;
pub mod shim;
pub mod state;
#[cfg(test)]
pub(crate) mod testutil;
pub mod verdict;
