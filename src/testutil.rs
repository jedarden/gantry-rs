//! Test-only helpers shared across the crate's unit tests.
//!
//! The test binary runs every `#[cfg(test)]` module in one process under the
//! default parallel harness, so anything process-wide (the current directory,
//! environment variables) is shared mutable state. Gantry's hermeticity rule
//! is that tests never touch it: repo operations take explicit paths
//! (`check_git_gate_in`, `RefPusher::push_in`, `Config::repo_config_path_in`),
//! and this module provides the tripwire that keeps it that way.

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict};
use std::cell::RefCell;
use std::time::Instant;

/// Deterministic stage values for decision-pipeline unit tests. A normal
/// originator/unjoined dispatch carries these nonzero values; Attach supplies
/// `(0, 0)` because it ran neither stage.
pub(crate) const RECORDING_PUSH_MS: u64 = 17;
pub(crate) const RECORDING_QUEUE_MS: u64 = 23;

/// One request observed by [`RecordingBackend`], including the opaque handle
/// it returned to the decision pipeline.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecordedSubmission {
    pub(crate) spec: RunSpec,
    pub(crate) handle: RunHandle,
}

/// One epoch-ref push observed by [`RecordingDispatch`]. Keeping the inputs
/// makes the fixture useful for more than counting calls: a dispatch test can
/// prove that the push was made for the run it later submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedPush {
    pub(crate) sha: String,
    pub(crate) run_id: String,
}

/// Deterministic in-process backend for decision-pipeline tests. It records
/// submissions and waits without spawning a process or touching the network.
#[derive(Debug)]
pub(crate) struct RecordingBackend {
    handle: RunHandle,
    submissions: RefCell<Vec<RecordedSubmission>>,
}

impl Default for RecordingBackend {
    fn default() -> Self {
        Self::new("recorded-originator")
    }
}

impl RecordingBackend {
    pub(crate) fn new(handle: &str) -> Self {
        Self {
            handle: RunHandle::new(handle),
            submissions: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn submissions(&self) -> Vec<RecordedSubmission> {
        self.submissions.borrow().clone()
    }
}

impl RemoteBackend for RecordingBackend {
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError> {
        let handle = self.handle.clone();
        self.submissions.borrow_mut().push(RecordedSubmission {
            spec: spec.clone(),
            handle: handle.clone(),
        });
        Ok(handle)
    }

    fn wait(&self, _handle: &RunHandle, _deadline: Instant) -> Result<Verdict, BackendError> {
        Ok(Verdict::Pass)
    }
}

/// Records both client-side epoch-ref pushes and backend submissions while
/// returning the stage durations a normal dispatch would stamp in its runlog.
#[derive(Debug)]
pub(crate) struct RecordingDispatch {
    pub(crate) backend: RecordingBackend,
    epoch_ref_pushes: RefCell<Vec<RecordedPush>>,
    push_duration_ms: u64,
    queue_duration_ms: u64,
}

impl Default for RecordingDispatch {
    fn default() -> Self {
        Self::new(RECORDING_PUSH_MS, RECORDING_QUEUE_MS)
    }
}

impl RecordingDispatch {
    pub(crate) fn new(push_duration_ms: u64, queue_duration_ms: u64) -> Self {
        Self::with_handle("recorded-originator", push_duration_ms, queue_duration_ms)
    }

    pub(crate) fn with_handle(handle: &str, push_duration_ms: u64, queue_duration_ms: u64) -> Self {
        Self {
            backend: RecordingBackend::new(handle),
            epoch_ref_pushes: RefCell::new(Vec::new()),
            push_duration_ms,
            queue_duration_ms,
        }
    }

    pub(crate) fn dispatch_originator(&self) -> Result<(RunHandle, u64, u64), i32> {
        self.record_ref_push("abc123", "recorded-run");
        let submitted = self
            .backend
            .submit(&RunSpec::new(
                "cargo",
                "test",
                Vec::new(),
                "file:///repo",
                "abc123",
                "",
            ))
            .map_err(|_| 1)?;
        Ok((submitted, self.push_duration_ms, self.queue_duration_ms))
    }

    pub(crate) fn record_ref_push(&self, sha: &str, run_id: &str) {
        self.epoch_ref_pushes.borrow_mut().push(RecordedPush {
            sha: sha.to_string(),
            run_id: run_id.to_string(),
        });
    }

    pub(crate) fn ref_pushes(&self) -> Vec<RecordedPush> {
        self.epoch_ref_pushes.borrow().clone()
    }

    pub(crate) fn epoch_ref_pushes(&self) -> u32 {
        self.epoch_ref_pushes.borrow().len() as u32
    }
}

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
