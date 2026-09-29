pub mod backend;
pub mod cli; // plan module layout: management-CLI diagnostics — why / explain / status, --json with schema_version
pub mod config;
pub mod decision;
pub mod doctor;
pub mod gate;
pub mod refs;
pub mod runlog;
pub mod shim;
pub mod state;
#[cfg(test)]
pub(crate) mod testutil;
