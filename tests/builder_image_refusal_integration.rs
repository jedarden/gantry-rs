// gantry — the builder-image parity refusal, end to end (plan Component 5,
// acceptance scenario 1, bead gantry-a6745d06).
//
// Proves, through the REAL process seams (no injected runner or inspector
// fakes — the fakes-backed wiring is pinned in src/backend/argo.rs's test
// module): when the submit-path parity preflight looks up the configured
// builder image and finds an `org.gantry.toolchain` label whose payload
// mismatches the repo's rust-toolchain.toml, gantry refuses that image —
// no Workflow manifest reaches kubectl — and the refusal hands the run to
// the local fallback ladder instead of failing it.
//
// The registry half of the seam is real too: the production
// `RegistryInspector` cascade (skopeo → crane → docker) discovers a mock
// `skopeo` on PATH and parses its answer through the real label
// normalization, so the mismatch is proven against the actual document
// shape skopeo prints and the actual label payload format
// `CapabilityLabel::to_label_value()` serializes — not a test-only
// shorthand for either.
//
// The mock kubectl records the argv it received and exits 99: a parity
// refusal must mean the workflow is never built or submitted, so as much
// as ONE kubectl invocation is a failure the record file makes visible.
// The control test arms the same seams with a *matching* label and shows
// the submission proceeding normally — the refusal is the mismatch, not
// broken tooling.
//
// PATH handling: `RegistryInspector` resolves its tools on PATH, and the
// production `ArgoBackend::new` offers no inspector seam (by design —
// that is the seam under test). So each test prepends a per-test tempdir
// holding its mock `skopeo` and restores PATH on drop. Process
// environment is per-process, and the only threads sharing it are this
// binary's, so a file-wide mutex serializing the PATH-touching tests
// keeps the suite hermetic under parallel `cargo test` (the crate's
// tripwire rule, src/testutil.rs). PATH is only ever *prepended to* —
// every other tool keeps resolving during the window.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use gantry::backend::argo::{ArgoBackend, ArgoConfig, ParityPreflight};
use gantry::backend::{RemoteBackend, RunHandle, RunSpec};
use gantry::config::Config;
use gantry::labels::{CapabilityLabel, TOOLCHAIN_LABEL_KEY};
use gantry::local::{run_fallback_program, FallbackContext};

/// Serializes every PATH-mutating test in this binary (see module docs).
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// Prepends `dir` to PATH for the guard's lifetime, restoring on drop —
/// including on a failing assert, so a test's mock never outlives its scope.
struct PathGuard {
    original: std::ffi::OsString,
}

impl PathGuard {
    fn prepend(dir: &Path) -> Self {
        let original = std::env::var_os("PATH").unwrap_or_default();
        let mutated = std::env::join_paths(
            std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&original)),
        )
        .expect("prepended PATH joins");
        std::env::set_var("PATH", &mutated);
        PathGuard { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.original);
    }
}

/// Write an executable mock into `dir` and return its path. The handle is
/// synced and dropped before the mock is ever exec'd (the ETXTBSY insurance
/// the other process-spawning suites also take).
fn write_mock(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = fs::File::create(&path).expect("create mock");
    file.write_all(body.as_bytes()).expect("write mock");
    file.sync_all().expect("sync mock");
    let mut perm = fs::metadata(&path).expect("stat mock").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm).expect("make mock executable");
    path
}

/// The label value a mismatching builder image advertises: the real
/// serialization of a capability claim, round-tripped through
/// [`CapabilityLabel::to_label_value`] so the mock serves exactly the bytes
/// a real registry label would carry.
fn label_value(channel: &str) -> String {
    CapabilityLabel {
        channel: channel.to_string(),
        pin: None,
        features: Default::default(),
    }
    .to_label_value()
}

/// A skopeo mock: records its argv (so the test can prove the production
/// inspector asked about the configured image), then prints an inspect
/// document in skopeo's real shape — `Labels` at the top level — carrying
/// the `org.gantry.toolchain` label with `channel`'s capability claim.
fn skopeo_mock(dir: &Path, image: &str, channel: &str) -> PathBuf {
    let mut labels = serde_json::Map::new();
    labels.insert(
        TOOLCHAIN_LABEL_KEY.to_string(),
        serde_json::Value::String(label_value(channel)),
    );
    let doc = serde_json::json!({ "Name": image, "Labels": labels });
    let argv_record = dir.join("skopeo-argv.txt");
    write_mock(
        dir,
        "skopeo",
        &format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$@\" > '{argv_record}'\n\
             cat <<'GANTRY_DOC'\n{doc}\nGANTRY_DOC\n",
            argv_record = argv_record.display(),
            doc = doc,
        ),
    )
}

/// A kubectl mock that records the argv it received and refuses everything —
/// a parity refusal must never let it be reached at all. Its error carries
/// the race-guard marker the retry helper treats as transient: the only way
/// this mock runs against a refused submission is the skopeo half of the
/// scenario losing the fresh-mock exec race and degrading to the warn-only
/// arm, and re-running the whole scenario is cheaper than failing on it.
fn recording_kubectl(dir: &Path) -> (PathBuf, PathBuf) {
    let record = dir.join("kubectl-argv.txt");
    let mock = write_mock(
        dir,
        "mock-kubectl",
        &format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$@\" > '{record}'\n\
             echo 'race-guard: mock kubectl reached (Text file busy retry)' >&2\n\
             exit 99\n",
            record = record.display(),
        ),
    );
    (mock, record)
}

/// A kubectl mock that consumes the piped manifest and answers like real
/// kubectl does on a successful `create -f -`.
fn creating_kubectl(dir: &Path, workflow: &str) -> (PathBuf, PathBuf) {
    let manifest = dir.join("manifest.json");
    let mock = write_mock(
        dir,
        "mock-kubectl",
        &format!(
            "#!/usr/bin/env bash\n\
             cat > '{manifest}'\n\
             echo 'workflow.argoproj.io/{workflow} created'\n",
            manifest = manifest.display(),
            workflow = workflow,
        ),
    );
    (mock, manifest)
}

/// The repo's pin file: `channel` in rust-toolchain.toml's real shape.
fn pin_file(dir: &Path, channel: &str) -> PathBuf {
    let path = dir.join("rust-toolchain.toml");
    fs::write(&path, format!("[toolchain]\nchannel = \"{channel}\"\n")).expect("write pin file");
    path
}

/// An `ArgoBackend` built the production way — `ArgoBackend::new`, so the
/// real `ProcessKubectl` and real `RegistryInspector` sit behind the seams —
/// with the parity preflight armed against `pin` and `image`.
fn backend_with(kubectl: &Path, pin: &Path, image: &str) -> ArgoBackend {
    ArgoBackend::new(ArgoConfig {
        kubectl_path: kubectl.to_str().expect("temp path is utf-8").to_string(),
        builder_image: Some(image.to_string()),
        parity: Some(ParityPreflight::new(pin.to_path_buf(), Vec::new())),
        ..ArgoConfig::default()
    })
}

/// The submission spec every test sends: the intercepted-run shape
/// (`cargo test` on this repo) the argo backend would carry remotely.
fn submit_spec() -> RunSpec {
    RunSpec::new(
        "cargo",
        "test",
        Vec::new(),
        "file:///fixture/repo.git",
        "0123456789abcdef0123456789abcdef01234567",
        "",
    )
}

/// Submit, absorbing the transient ETXTBSY window a freshly-written mock can
/// hit under the parallel harness (same insurance as
/// tests/argo_backend_integration.rs): retry while the error is the transient
/// shape — a mock exec losing the race, or the recording kubectl's
/// race-guard marker proving the skopeo cascade lost it and the warn-only
/// arm submitted in its place.
fn submit_with_retry(
    backend: &ArgoBackend,
    spec: &RunSpec,
) -> Result<RunHandle, gantry::backend::BackendError> {
    let retryable =
        |reason: &str| reason.contains("Text file busy") || reason.contains("race-guard");
    let mut last = None;
    for attempt in 0..5 {
        match backend.submit(spec) {
            Err(e) if attempt < 4 && retryable(&e.reason) => {
                last = Some(e);
                thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
            }
            other => return other,
        }
    }
    Err(last.expect("submit ran at least once"))
}

/// THE scenario (plan Component 5, acceptance scenario 1): the repo pins
/// `stable`, the configured builder image advertises `nightly` in its real
/// `org.gantry.toolchain` label — the submission is refused before anything
/// remote happens, and kubectl is never invoked, so no Workflow manifest is
/// built or submitted. The caller's infra ladder (asserted by the fallback
/// test below) lands the run locally from exactly this error.
#[test]
fn mismatched_builder_image_is_refused_before_kubectl_is_touched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _lock = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _path = PathGuard::prepend(tmp.path());

    let image = "registry.test/gantry-builder:miss";
    let _skopeo = skopeo_mock(tmp.path(), image, "nightly");
    let (kubectl, kubectl_record) = recording_kubectl(tmp.path());
    let pin = pin_file(tmp.path(), "stable");
    let backend = backend_with(&kubectl, &pin, image);

    let err = submit_with_retry(&backend, &submit_spec())
        .expect_err("a stable repo vs a nightly image must refuse the submission");

    // The refusal names the decision, both channels, the image to relabel,
    // and the fallback that follows — the payload survived the real registry
    // round trip, so "nightly" here can only have come from the mock's label.
    assert!(
        err.reason.contains("refusing remote run"),
        "error carries the refusal: {}",
        err.reason
    );
    assert!(
        err.reason.contains(image),
        "the image is named: {}",
        err.reason
    );
    assert!(
        err.reason
            .contains("repo pins toolchain channel \"stable\""),
        "the repo's pin is named: {}",
        err.reason
    );
    assert!(
        err.reason.contains("the image advertises \"nightly\""),
        "the image's label payload is named: {}",
        err.reason
    );
    assert!(
        err.reason.contains("falling back to local execution"),
        "the fallback is explained: {}",
        err.reason
    );

    // The production inspector really ran the cascade through the mock —
    // asked with the docker:// ref it builds from the configured image.
    let skopeo_argv =
        fs::read_to_string(tmp.path().join("skopeo-argv.txt")).expect("skopeo was invoked");
    assert!(
        skopeo_argv.contains(&format!("docker://{image}")),
        "the inspector must query the configured image, got: {skopeo_argv}",
    );

    // And kubectl was never touched: the refusal precedes the manifest, so
    // there is nothing in the cluster for a retry ladder to duplicate.
    assert!(
        !kubectl_record.exists(),
        "a refused submission must not reach kubectl — got argv: {}",
        fs::read_to_string(&kubectl_record).unwrap_or_default()
    );
}

/// The control: the same seams, a *matching* label — the preflight is
/// silent and the submission proceeds exactly as if it were disarmed, one
/// `create -f -` carrying the workflow (and the image) to kubectl. The
/// refusal above is the mismatch, not an artifact of the mock plumbing.
#[test]
fn matching_builder_image_submits_through_the_real_label_lookup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _lock = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _path = PathGuard::prepend(tmp.path());

    let image = "registry.test/gantry-builder:ok";
    let _skopeo = skopeo_mock(tmp.path(), image, "stable");
    let (kubectl, manifest_path) = creating_kubectl(tmp.path(), "gantry-match-1");
    let pin = pin_file(tmp.path(), "stable");
    let backend = backend_with(&kubectl, &pin, image);

    let handle = submit_with_retry(&backend, &submit_spec()).expect("a parity match submits");
    assert_eq!(handle, RunHandle::new("gantry-match-1"));

    // The manifest that went out the real pipe: the generated name comes
    // from the config, and the builder image rides it as the template
    // parameter the cluster-side run will use. Matched whitespace-free —
    // kubectl's pretty-print style is not part of the contract.
    let manifest =
        fs::read_to_string(&manifest_path).expect("kubectl received the workflow manifest");
    let compact: String = manifest.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("\"generateName\":\"gantry-\""),
        "manifest: {manifest}",
    );
    assert!(
        compact.contains(image),
        "the builder image parameter is carried: {manifest}",
    );
}

/// The handoff the refusal promises: the decision layer's submit-error arm
/// feeds exactly this error to the local fallback ladder
/// (`resolve_and_fall_back` → [`gantry::local::run_fallback_program`]), and
/// the local outcome becomes the run's verdict — a refused remote run is a
/// local run, not a failed one. This drives the real ladder end to end with
/// the real refusal text and asserts the wrapped program's exit code wins.
#[test]
fn refusal_hands_the_run_to_the_local_fallback_ladder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _lock = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _path = PathGuard::prepend(tmp.path());

    let image = "registry.test/gantry-builder:miss";
    let _skopeo = skopeo_mock(tmp.path(), image, "nightly");
    let (kubectl, kubectl_record) = recording_kubectl(tmp.path());
    let pin = pin_file(tmp.path(), "stable");
    let backend = backend_with(&kubectl, &pin, image);

    let err = submit_with_retry(&backend, &submit_spec())
        .expect_err("the mismatch refuses the submission");

    // The local tail the decision layer runs: resolve the wrapped program
    // (the shim's business, upstream of this call) and hand it to the ladder
    // with the submit failure as the infra reason. The program writes its
    // marker and exits 3 — a deliberately non-zero outcome the ladder must
    // return verbatim (INV-3), proving the local result is the verdict.
    let marker = tmp.path().join("local-ran.marker");
    let local_program = write_mock(
        tmp.path(),
        "local-program",
        &format!(
            "#!/usr/bin/env bash\n\
             echo ran > '{marker}'\n\
             exit 3\n",
            marker = marker.display(),
        ),
    );

    // Tier-0 defaults, with the fallback wait bounded so a busy semaphore
    // degrades loudly in seconds rather than queueing the test for an hour.
    let mut config = Config::hardcoded();
    config.local.fallback_wait_secs = 2;

    let exit = run_fallback_program(
        &config,
        &local_program,
        &[],
        &format!("submit failed: {}", err.reason),
        &FallbackContext {
            runlog: None,
            run_id: "parity-refusal-fallback",
            gate_ms: 0,
            push_ms: 0,
        },
    );

    // The local outcome IS the verdict: exit 3 comes back verbatim, and the
    // program really ran locally. The run did not die on the refusal.
    assert_eq!(exit, 3, "the wrapped program's exit code wins (INV-3)");
    assert!(
        marker.exists(),
        "the fallback really executed the program locally"
    );
    assert!(
        !kubectl_record.exists(),
        "even after the local ladder, the refused submission never reached kubectl",
    );
}
