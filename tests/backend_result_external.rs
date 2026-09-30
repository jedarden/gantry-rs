// gantry — external-use contract for BackendResult (gantry-7ddcb5d3).
//
// The alias must be importable from outside the backend module — here from
// outside the crate entirely — and usable alongside the RemoteBackend trait,
// including through a trait object with a minimal in-test stub backend.

use gantry::backend::{
    BackendError, BackendResult, RemoteBackend, RunHandle, RunSpec, RunStatus, Verdict,
};

/// A stub answering from the handle string alone, as an external consumer of
/// the interface would implement it. No shared state, filesystem, or
/// environment — parallel-safe under the default test harness.
struct ExternalStub;

impl RemoteBackend for ExternalStub {
    fn submit(&self, spec: &RunSpec) -> BackendResult<RunHandle> {
        Ok(RunHandle::new(&format!("ext-{}", spec.sha)))
    }

    fn status(&self, h: &RunHandle) -> BackendResult<RunStatus> {
        match h.handle.as_str() {
            "ext-pass" | "ext-fail" => Ok(RunStatus::Completed),
            other => Err(BackendError::new(&format!("no such run {other:?}"))),
        }
    }

    fn wait(&self, h: &RunHandle, deadline: std::time::Instant) -> BackendResult<Verdict> {
        if std::time::Instant::now() > deadline {
            return Err(BackendError::new("deadline passed"));
        }
        match h.handle.as_str() {
            "ext-pass" => Ok(Verdict::Pass),
            "ext-fail" => Ok(Verdict::GateFailure),
            other => Err(BackendError::new(&format!("no such run {other:?}"))),
        }
    }
}

fn stub_spec(sha: &str) -> RunSpec {
    RunSpec::new(
        "cargo",
        "test",
        vec![],
        "file:///tmp/gantry-external",
        sha,
        "",
    )
}

#[test]
fn external_consumer_drives_the_trait_object_lifecycle() {
    let backend: Box<dyn RemoteBackend> = Box::new(ExternalStub);

    let h = backend.submit(&stub_spec("pass")).expect("submit");
    assert_eq!(h.handle, "ext-pass");
    assert_eq!(backend.status(&h).expect("status"), RunStatus::Completed);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    assert_eq!(backend.wait(&h, deadline).expect("wait"), Verdict::Pass);

    let h = backend.submit(&stub_spec("fail")).expect("submit");
    assert_eq!(
        backend.wait(&h, deadline).expect("wait"),
        Verdict::GateFailure
    );
}

#[test]
fn external_consumer_sees_alias_errors_as_backend_errors() {
    let backend: Box<dyn RemoteBackend> = Box::new(ExternalStub);
    let err = backend
        .status(&RunHandle::new("ext-never-submitted"))
        .expect_err("unknown handle errors");
    assert_eq!(
        err,
        BackendError::new("no such run \"ext-never-submitted\"")
    );
}

#[test]
fn external_alias_is_transparent_with_the_expanded_result() {
    // A BackendResult passes where the spelled-out Result is expected.
    fn classify(r: Result<Verdict, BackendError>) -> BackendResult<Verdict> {
        r
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let backend: Box<dyn RemoteBackend> = Box::new(ExternalStub);
    let h = backend.submit(&stub_spec("pass")).expect("submit");
    assert_eq!(classify(backend.wait(&h, deadline)), Ok(Verdict::Pass));
}
