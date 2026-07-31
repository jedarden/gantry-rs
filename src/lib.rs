pub mod backend;
pub mod config;
pub mod decision;
pub mod gate;
pub mod refs;
pub mod shim;

/// Test-only helpers shared across the unit-test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::{Mutex, MutexGuard};

    /// One process-wide lock for tests that change the current directory.
    ///
    /// The cwd is process state, so a test that chdirs into a temp dir and then
    /// deletes it can make an unrelated test's `Command::spawn` fail with ENOENT
    /// (a spawn from a deleted cwd). `gate` and `refs` each held their own mutex,
    /// which serialized each module against itself but not against the other —
    /// this is the shared one both take.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the cwd lock, ignoring poisoning.
    ///
    /// A panicking test poisons the mutex; propagating that would bury the one
    /// real failure under a cascade of unrelated `PoisonError`s.
    pub(crate) fn lock_cwd() -> MutexGuard<'static, ()> {
        CWD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
