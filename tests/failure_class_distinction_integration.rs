// gantry — a broken build and red tests land in distinct failure classes
// (gantry-f03bc20f; parent gantry-9d3c520c, plan Component 5 acceptance
// scenario 3).
//
// The taxonomy exists so downstream consumers can branch on `failure_class`
// in verdict.json / runs.jsonl and tell a compile error from a failing test
// without parsing logs. The per-fixture pin lives in the shared corpus's
// classify tests (src/verdict.rs); this integration test consumes the same
// corpus the way a downstream consumer would and pins the DISTINCTION
// itself: the compile-error fixture and the test-failure fixture classify
// to different classes, each the expected one, on the kebab wire form the
// verdict documents carry — and they stay distinct where the raw evidence
// collides, at cargo's shared exit 101, which both a broken build and a
// failed test binary produce.

use gantry::verdict::FailureClass;
use serde::Deserialize;

/// One fixture of the shared producer-reference corpus: exactly the inputs
/// the producer archives (exit code, instrumentation, the two streams) plus
/// the class both implementations must stamp.
#[derive(Debug, Deserialize)]
struct CorpusEntry {
    name: String,
    /// What the fixture proves, rendered on failure.
    why: String,
    /// The raw cargo exit code (0-255 shell domain).
    exit_code: i32,
    /// False when the caller chose their own `--message-format`.
    instrumented: bool,
    /// The archived cargo JSON protocol stream.
    messages: String,
    /// The human-visible run output.
    run_log: String,
    /// The kebab-case class both implementations must stamp, null where the
    /// classifiable window leaves the class unset.
    expected_class: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Corpus {
    entries: Vec<CorpusEntry>,
}

/// The shared corpus, exactly as the classifier's unit tests and the
/// producer-parity test read it.
fn corpus() -> Corpus {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/failure-class-corpus.json"
    )))
    .expect("the shared producer-reference corpus must parse")
}

/// The named fixture of the corpus.
fn entry<'a>(corpus: &'a Corpus, name: &str) -> &'a CorpusEntry {
    corpus
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("the corpus carries the {name:?} fixture"))
}

/// Classify a fixture with exactly the inputs the producer archives.
fn classify(fixture: &CorpusEntry) -> Option<FailureClass> {
    FailureClass::classify(
        fixture.exit_code,
        fixture.instrumented,
        &fixture.messages,
        &fixture.run_log,
    )
}

/// The class the corpus entry pins, parsed off its kebab wire form.
fn expected_class(fixture: &CorpusEntry) -> Option<FailureClass> {
    fixture.expected_class.as_deref().map(|raw| {
        FailureClass::from_kebab(raw).unwrap_or_else(|| {
            panic!(
                "corpus entry {} names an unknown class {raw:?}",
                fixture.name
            )
        })
    })
}

/// The kebab-case wire form a verdict document carries for a class.
fn wire(class: Option<FailureClass>) -> String {
    serde_json::to_value(class)
        .expect("a class serializes")
        .as_str()
        .expect("the wire form of a class is a string")
        .to_string()
}

/// Scenario 3: a compile error and a failing test produce distinct failure
/// classes — each fixture of the pair classifies to its expected class, the
/// two classes differ, and the difference survives the kebab wire form
/// downstream consumers actually read.
#[test]
fn a_broken_build_and_red_tests_land_in_distinct_classes() {
    let corpus = corpus();
    let broken = entry(&corpus, "compile-error-from-the-protocol-stream");
    let red = entry(&corpus, "test-failure-is-the-fallthrough-class");

    assert_eq!(
        classify(broken),
        expected_class(broken),
        "the broken build must classify as its corpus pin: {}",
        broken.why
    );
    assert_eq!(
        classify(red),
        expected_class(red),
        "the red suite must classify as its corpus pin: {}",
        red.why
    );
    assert_eq!(
        classify(broken),
        Some(FailureClass::CompileError),
        "a broken build is a compile error: {}",
        broken.why
    );
    assert_eq!(
        classify(red),
        Some(FailureClass::TestFailure),
        "red tests are a plain test failure: {}",
        red.why
    );
    assert_ne!(
        classify(broken),
        classify(red),
        "a broken build and red tests must land in different classes — a \
         consumer branching on the class must be able to tell them apart"
    );

    // The wire form is the contract downstream consumers read off
    // verdict.json / runs.jsonl; the distinction must survive it.
    assert_eq!(wire(classify(broken)), "compile-error");
    assert_eq!(wire(classify(red)), "test-failure");
}

/// The distinction does not lean on the exit code: cargo exits 101 for a
/// broken build AND for a failed test binary, and the corpus carries the
/// red-test twin of the compile-error fixture at that same exit code. The
/// archived protocol stream decides — not `$?`.
#[test]
fn the_distinction_survives_cargos_shared_exit_code() {
    let corpus = corpus();
    let broken = entry(&corpus, "compile-error-from-the-protocol-stream");
    let red_twin = entry(&corpus, "test-failure-shares-the-compile-error-exit-code");

    assert_eq!(
        broken.exit_code, red_twin.exit_code,
        "the twin fixtures share cargo's exit code — that collision is the \
         point of the pair"
    );
    assert_eq!(
        classify(red_twin),
        expected_class(red_twin),
        "the red-test twin must classify as its corpus pin: {}",
        red_twin.why
    );
    assert_eq!(
        classify(red_twin),
        Some(FailureClass::TestFailure),
        "at the compile-error exit code, red tests are still a plain test \
         failure: {}",
        red_twin.why
    );
    assert_ne!(
        classify(broken),
        classify(red_twin),
        "same exit code, different classes: the archived evidence decides, \
         never the exit code"
    );
}
