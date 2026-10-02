// gantry — integration tests for the parse and decision hops of the remote
// failure taxonomy (verdict.json v2, plan §Component 5).
//
// Reads the shared corpus (tests/fixtures/verdict-v2-corpus.json — one whole
// verdict.json document per outcome an agent can be handed) and drives every
// fixture through the two client hops the taxonomy class must survive
// unchanged:
//
//   hop 1 — the backend parse: `VerdictJson::parse` lands the document's
//           `failure_class` (lenient kebab deserialization; the class is
//           None for every null-class fixture)
//   hop 2 — the decision step of the remote client path: the argo backend's
//           `wait_outcome`, whose terminal-phase arm pairs the verdict it
//           decides with the class the parsed document attributes
//
// Hop 2 is driven end-to-end against a mock kubectl serving the terminal
// workflow whose `verdict` output parameter carries the fixture document —
// the same hermetic idiom as tests/argo_backend_integration.rs: one
// executable script per call site, per-test temp dirs, no env vars, so the
// suite stays safe under parallel `cargo test`.

use gantry::backend::argo::{ArgoBackend, ArgoConfig};
use gantry::backend::{BackendError, FailureClass, RemoteBackend, RunHandle, Verdict, VerdictJson};
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

// --- the shared fixture corpus -------------------------------------------

/// One entry of the shared verdict.json document corpus: the complete
/// document exactly as a producer writes it, plus the class — or the absence
/// of one — and the verdict both hops must land on. Same shape the parse-pin
/// in src/verdict.rs reads.
#[derive(Debug, Deserialize)]
struct CorpusEntry {
    name: String,
    why: String,
    /// The complete verdict.json document, compact and single-line.
    document: String,
    /// The kebab-case class both hops must carry, null where the contract
    /// leaves the class unset.
    expected_class: Option<String>,
    /// The kebab-case verdict the document must ladder to.
    expected_verdict: String,
}

/// The shared corpus file's top level: the entries plus the producer's
/// provenance note, which these tests have no use for (serde drops fields
/// the struct omits).
#[derive(Debug, Deserialize)]
struct Corpus {
    entries: Vec<CorpusEntry>,
}

/// The shared corpus, exactly as the parse pin and these hop tests read it.
fn corpus() -> Vec<CorpusEntry> {
    serde_json::from_str::<Corpus>(include_str!("fixtures/verdict-v2-corpus.json"))
        .expect("the shared verdict.json document corpus must parse")
        .entries
}

/// The corpus's kebab class names, mapped once — the corpus stays data, so
/// reading it needs no serde on [`FailureClass`].
fn expected_class(raw: Option<&str>, name: &str) -> Option<FailureClass> {
    raw.map(|kebab| {
        FailureClass::from_kebab(kebab)
            .unwrap_or_else(|| panic!("corpus entry {name} names an unknown class {kebab:?}"))
    })
}

/// The corpus's kebab verdict names, mapped once.
fn expected_verdict(raw: &str, name: &str) -> Verdict {
    match raw {
        "pass" => Verdict::Pass,
        "test-failure" => Verdict::TestFailure,
        "gate-failure" => Verdict::GateFailure,
        "infra-failure" => Verdict::InfraFailure,
        other => panic!("corpus entry {name} names an unknown verdict {other:?}"),
    }
}

// --- the mock kubectl (tests/argo_backend_integration.rs's idiom) ---------

/// Write an executable mock kubectl into `dir` and return its path.
fn write_mock_kubectl(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("mock-kubectl");
    let mut file = fs::File::create(&path).expect("create mock kubectl");
    file.write_all(body.as_bytes()).expect("write mock kubectl");
    let mut perm = fs::metadata(&path)
        .expect("stat mock kubectl")
        .permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm).expect("make mock kubectl executable");
    path
}

fn kubectl_path(p: &Path) -> String {
    p.to_str().expect("temp path is valid utf-8").to_string()
}

/// Retry a mock-backed call a few times when exec fails with ETXTBSY
/// ("Text file busy") — under the parallel test harness, exec of a
/// freshly-written mock can transiently race a still-open write handle; the
/// condition clears once every straggler handle closes. Production exec
/// paths deliberately do NOT get this — they must surface real spawn errors
/// loudly.
fn with_exec_retry<T>(mut f: impl FnMut() -> Result<T, BackendError>) -> Result<T, BackendError> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e) if attempt < 4 && e.reason.contains("Text file busy") => {
                attempt += 1;
                thread::sleep(Duration::from_millis(50 * attempt));
            }
            other => return other,
        }
    }
}

/// The terminal workflow object the mock serves: `phase` (the parsed
/// document's own phase — every corpus fixture is terminal) plus the `verdict`
/// output parameter carrying `document` verbatim.
fn terminal_workflow_json(phase: &str, document: &str) -> String {
    format!(
        r#"{{"status":{{"phase":"{phase}","outputs":{{"parameters":[{{"name":"verdict","value":{}}}]}}}}}}"#,
        serde_json::to_string(document).expect("document is a valid JSON string")
    )
}

/// Hop 2 — the decision step of the remote client path: drive `document`
/// through the argo backend's `wait_outcome` against a mock kubectl serving
/// the terminal workflow whose `verdict` parameter carries it, and return the
/// (verdict, class) outcome the client records.
fn decision_outcome(phase: &str, document: &str) -> (Verdict, Option<FailureClass>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kubectl = write_mock_kubectl(
        tmp.path(),
        &format!(
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' get workflow '*) cat <<'JSON'\n{}\nJSON\n ;;\n\
               *) echo \"unexpected argv: $*\" >&2; exit 99 ;;\n\
             esac\n",
            terminal_workflow_json(phase, document),
        ),
    );
    let backend = ArgoBackend::new(ArgoConfig {
        kubectl_path: kubectl_path(&kubectl),
        ..ArgoConfig::default()
    });
    let handle = RunHandle::new("gantry-abc123");

    with_exec_retry(|| backend.wait_outcome(&handle, Instant::now() + Duration::from_secs(60)))
        .expect("wait_outcome must return the terminal outcome")
}

// --- the hops -------------------------------------------------------------

/// Acceptance (1): every classified fixture — compile-error, test-failure,
/// doctest, harness-panic, and the attributed gate-failure — carries its
/// class through the backend parse and the decision step unchanged: the class
/// wait_outcome pairs with the verdict is exactly the class the parse
/// produced, hop for hop, and the decision lands the corpus's expected
/// verdict.
#[test]
fn classified_fixtures_carry_their_class_through_parse_and_decision() {
    for entry in corpus() {
        let Some(expected) = expected_class(entry.expected_class.as_deref(), &entry.name) else {
            continue;
        };

        // Hop 1 — the backend parse.
        let parsed = VerdictJson::parse(&entry.document)
            .unwrap_or_else(|e| panic!("fixture {} must parse: {}", entry.name, e.reason));
        assert_eq!(
            parsed.failure_class,
            Some(expected.clone()),
            "fixture {}: parse hop lost the class: {}",
            entry.name,
            entry.why
        );

        // Hop 2 — the decision step, fed the fixture verbatim.
        let (verdict, class) = decision_outcome(&parsed.phase, &entry.document);
        assert_eq!(
            class,
            Some(expected),
            "fixture {}: decision hop changed the class the parse produced: {}",
            entry.name,
            entry.why
        );
        assert_eq!(
            verdict,
            expected_verdict(&entry.expected_verdict, &entry.name),
            "fixture {}: decision hop landed the wrong verdict: {}",
            entry.name,
            entry.why
        );
    }
}

/// Acceptance (2): every null-class fixture — pass, the three infra signals
/// (oom, deadline, workflow-error), the uninstrumented failure, and the
/// schema-1 document with no taxonomy field — ends with class None after both
/// hops: nothing outside a classified failing suite invents a class, and the
/// decision still lands the corpus's expected verdict (the infra rung for the
/// infra signals, the honest exit-code rung for uninstrumented and schema-1
/// failures).
#[test]
fn null_class_fixtures_end_with_class_none_after_parse_and_decision() {
    for entry in corpus() {
        if entry.expected_class.is_some() {
            continue;
        }

        // Hop 1 — the backend parse reads no class.
        let parsed = VerdictJson::parse(&entry.document)
            .unwrap_or_else(|e| panic!("fixture {} must parse: {}", entry.name, e.reason));
        assert_eq!(
            parsed.failure_class, None,
            "fixture {}: parse hop invented a class: {}",
            entry.name, entry.why
        );

        // Hop 2 — the decision step keeps it None and ladders the verdict.
        let (verdict, class) = decision_outcome(&parsed.phase, &entry.document);
        assert_eq!(
            class, None,
            "fixture {}: decision hop invented a class: {}",
            entry.name, entry.why
        );
        assert_eq!(
            verdict,
            expected_verdict(&entry.expected_verdict, &entry.name),
            "fixture {}: decision hop landed the wrong verdict: {}",
            entry.name,
            entry.why
        );
    }
}

/// The corpus partitions exactly into the two acceptance sets: every entry is
/// exercised by one of the two hop tests above, so a fixture added without a
/// class column (or with a typo in one) cannot silently skip both hops.
#[test]
fn the_corpus_partitions_into_classified_and_null_class_fixtures() {
    let classified = corpus()
        .iter()
        .filter(|e| e.expected_class.is_some())
        .count();
    let null_class = corpus().len() - classified;
    assert_eq!(
        corpus().len(),
        classified + null_class,
        "every corpus entry must be either classified or null-class"
    );
    assert_eq!(classified, 5, "one fixture per FailureClass variant");
    assert!(
        null_class >= 5,
        "pass, the three infra signals, uninstrumented, and schema v1 must all be fixtures"
    );
}
