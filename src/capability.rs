// gantry — builder-image capability acquisition (plan Component 5, part 2).
//
// Given the configured builder image reference, find out what the image can
// do: retrieve its OCI labels, pull the `org.gantry.toolchain` payload out of
// them, and parse it into the [`CapabilityLabel`] schema. This is the
// acquisition half of the parity preflight — the decision that compares the
// result against the repo's rust-toolchain.toml is a later sibling of the
// same split, and the schema itself is part 1 (`crate::labels`).
//
// The lookup sits behind the injectable [`ImageInspector`] trait so tests
// substitute a fake inspector; the production [`RegistryInspector`] shells
// out to a read-only registry query (`skopeo inspect --format json`, with
// `crane config` and `docker image inspect` as fallbacks). Every probe is
// strictly read-only: no pull, no push, no daemon mutation — an image label
// is metadata the registry already serves.
//
// The failure posture is the bead's contract: lookup failure, missing
// tooling, and an unlabeled image all map to an explicit
// [`ImageCapability::Unknown`] carrying a reason string for the warn-only
// message — never a guess. A capability is [`ImageCapability::Known`] only
// when the image published the label and the payload parsed.

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;

use crate::labels::{CapabilityLabel, TOOLCHAIN_LABEL_KEY};

/// The `org.gantry.` namespace every gantry capability label lives under.
/// An image whose labels carry no key with this prefix is *unlabeled* — one
/// of the explicit-Unknown cases — even if it publishes plenty of other
/// labels. The namespace is open; unknown keys ride along and are ignored.
pub const GANTRY_LABEL_PREFIX: &str = "org.gantry.";

/// The read-only container tools the [`RegistryInspector`] probes, in try
/// order. `skopeo` first because it queries the registry directly (the
/// registry's label is the truth a fresh build would get, not whatever a
/// local daemon last pulled); `crane` is the same registry fetch in a
/// smaller binary; `docker` last because it answers from the local daemon —
/// still read-only (`image inspect`), just potentially stale.
const LABEL_TOOLS: &[&str] = &["skopeo", "crane", "docker"];

/// The OCI labels an image publishes, as far as the lookup could read them.
///
/// A wrapper rather than a bare map so the org.gantry questions ("is this
/// image labeled at all?", "what does its toolchain label say?") have one
/// authoritative spelling next to the data they read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLabels {
    labels: BTreeMap<String, String>,
}

impl ImageLabels {
    /// Wrap a raw label map (all values are strings — OCI label values are).
    pub fn from_map(labels: BTreeMap<String, String>) -> Self {
        ImageLabels { labels }
    }

    /// The whole map, for diagnostics that want to name what *was* there.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// One label's value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// Whether the image publishes anything under `org.gantry.` — the
    /// labeled / unlabeled line the preflight's warn-only posture hinges on.
    pub fn gantry_labeled(&self) -> bool {
        self.labels
            .keys()
            .any(|key| key.starts_with(GANTRY_LABEL_PREFIX))
    }
}

/// Why a builder image's capability lookup produced no labels at all.
///
/// Typed rather than a string so the two shapes can be told apart by
/// consumers that care (a `doctor` check wants to say "install a registry
/// tool", not "the registry said no"), while [`capability_of`] flattens both
/// into an [`ImageCapability::Unknown`] reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupFailure {
    /// No read-only registry tool resolves on this system — the lookup could
    /// not even start. Names everything that was looked for.
    MissingTooling { tried: Vec<String> },
    /// A tool ran and could not answer (non-zero exit, or output that is not
    /// a label map). Carries every tool's failure, semicolon-joined.
    LookupError { detail: String },
}

impl fmt::Display for LookupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LookupFailure::MissingTooling { tried } => write!(
                f,
                "no read-only registry tool on PATH (tried {})",
                tried.join(", ")
            ),
            LookupFailure::LookupError { detail } => write!(f, "label lookup failed: {detail}"),
        }
    }
}

impl std::error::Error for LookupFailure {}

/// What acquisition concluded about a builder image's capabilities.
///
/// Two variants only, per the contract: the image's capability is either
/// known (it published a parseable `org.gantry.toolchain` payload) or
/// explicitly unknown, with the reason the warn-only message will carry.
/// There is deliberately no third "assume it's fine" variant — everything
/// the lookup cannot prove lands in [`ImageCapability::Unknown`], because
/// the preflight's refusal later needs a *provable* mismatch and must never
/// manufacture one from a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCapability {
    /// The image publishes `org.gantry.toolchain` and the payload parsed.
    Known {
        /// The parsed capability claim: channel, optional pin, feature set.
        label: CapabilityLabel,
    },
    /// The image's capability could not be established. The reason is the
    /// warn-only message's body: what was tried and why it came up empty.
    Unknown {
        /// Human-readable reason; never empty.
        reason: String,
    },
}

/// The injectable seam: an image-label source.
///
/// Production wires [`RegistryInspector`] (shell-outs to read-only registry
/// tools); tests wire a fake carrying a canned answer. The trait is total —
/// every failure is a typed [`LookupFailure`], never a panic or an error
/// channel — so callers match two variants at [`capability_of`] and are done.
pub trait ImageInspector {
    /// Fetch the image's OCI labels, or say why that cannot be done.
    fn labels(&self, image: &str) -> Result<ImageLabels, LookupFailure>;
}

/// Acquire a builder image's capability: look up its labels through
/// `inspector`, then parse the `org.gantry.toolchain` payload out of them.
///
/// The explicit-Unknown ladder, outermost first:
/// 1. the lookup itself fails ([`LookupFailure`] — missing tooling or a
///    failed registry query) → `Unknown` with that failure as the reason;
/// 2. the image publishes no `org.gantry.*` label at all → `Unknown`
///    ("unlabeled");
/// 3. the image publishes `org.gantry.*` labels but not the toolchain key →
///    `Unknown` naming the missing key (the namespace is open; other keys
///    are not capabilities);
/// 4. the payload is present but does not parse
///    ([`LabelParseError`](crate::labels::LabelParseError)) → `Unknown`
///    with the typed parse error.
///
/// Only a payload that parses yields [`ImageCapability::Known`].
pub fn capability_of(inspector: &dyn ImageInspector, image: &str) -> ImageCapability {
    let labels = match inspector.labels(image) {
        Ok(labels) => labels,
        Err(failure) => {
            return ImageCapability::Unknown {
                reason: failure.to_string(),
            }
        }
    };
    classify(&labels)
}

/// Sort a successful lookup into the Unknown ladder or a known capability.
fn classify(labels: &ImageLabels) -> ImageCapability {
    if !labels.gantry_labeled() {
        return ImageCapability::Unknown {
            reason: format!(
                "the image publishes no {GANTRY_LABEL_PREFIX}* labels — nothing to compare against"
            ),
        };
    }
    let Some(raw) = labels.get(TOOLCHAIN_LABEL_KEY) else {
        return ImageCapability::Unknown {
            reason: format!(
                "the image publishes {GANTRY_LABEL_PREFIX}* labels but no {TOOLCHAIN_LABEL_KEY} \
                 — no toolchain claim to compare against"
            ),
        };
    };
    match CapabilityLabel::parse(raw) {
        Ok(label) => ImageCapability::Known { label },
        Err(why) => ImageCapability::Unknown {
            reason: format!("the {TOOLCHAIN_LABEL_KEY} label does not parse: {why}"),
        },
    }
}

/// The production [`ImageInspector`]: shells out to read-only registry
/// queries, trying [`LABEL_TOOLS`] in order, first answer wins.
///
/// No state, no daemon assumptions: each tool is probed for existence on
/// PATH before it is spawned, and a tool that is absent is skipped rather
/// than an error — a box with only `docker` still gets a lookup.
#[derive(Debug, Clone, Copy, Default)]
pub struct RegistryInspector;

impl ImageInspector for RegistryInspector {
    fn labels(&self, image: &str) -> Result<ImageLabels, LookupFailure> {
        inspect_via(&tool_on_path, &run_tool, image)
    }
}

/// Whether a tool name resolves on PATH to a file. A plain scan — the probe
/// must not spawn anything to learn this, and a directory that merely shares
/// the name is not a tool.
fn tool_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// Run one registry tool, returning its stdout or a short failure reason.
fn run_tool(tool: &str, argv: &[String]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(argv)
        .output()
        .map_err(|e| format!("cannot spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "exited {}: {}",
            output.status,
            shorten(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A tool-availability answer: does this registry tool's name resolve here?
type ToolAvailable<'a> = &'a dyn Fn(&str) -> bool;

/// A tool run: the argv from [`inspect_argv`] in, stdout or a failure
/// reason out.
type ToolRun<'a> = &'a dyn Fn(&str, &[String]) -> Result<String, String>;

/// The cascade with its environment injected: `available` says whether a
/// tool's name resolves here, `run` executes it. The first tool that answers
/// a *readable* label map wins; a tool that cannot run, or that prints an
/// unreadable answer, has not answered — its failure joins the aggregate and
/// the cascade moves on. Production wires PATH lookup and process spawn;
/// tests wire fakes, keeping the try-order and the failure aggregation
/// unit-testable without touching the real environment.
fn inspect_via(
    available: ToolAvailable<'_>,
    run: ToolRun<'_>,
    image: &str,
) -> Result<ImageLabels, LookupFailure> {
    let mut missing: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for &tool in LABEL_TOOLS {
        if !available(tool) {
            missing.push(tool.to_string());
            continue;
        }
        match run(tool, &inspect_argv(tool, image)) {
            Ok(stdout) => match normalize_labels(tool, &stdout) {
                Ok(labels) => return Ok(ImageLabels::from_map(labels)),
                // A tool that answers garbage has not answered: its shape
                // problem joins the failures and the cascade moves on.
                Err(why) => failures.push(format!("{tool} printed unreadable label output: {why}")),
            },
            Err(why) => failures.push(format!("{tool} {why}")),
        }
    }
    if failures.is_empty() {
        Err(LookupFailure::MissingTooling { tried: missing })
    } else {
        // At least one tool ran; what it said outweighs which others were
        // absent, but the absent ones still ride along for the reason text.
        let mut detail = failures.join("; ");
        if !missing.is_empty() {
            detail.push_str(&format!(" (also absent: {})", missing.join(", ")));
        }
        Err(LookupFailure::LookupError { detail })
    }
}

/// The argv each registry tool is driven with. All three answers normalize
/// to a label map (see [`normalize_labels`]): skopeo prints its inspect
/// document with `Labels` at the top level, `crane config` prints the OCI
/// config blob whose `config.Labels` carries the labels, and docker's
/// `--format` prints the label object itself.
fn inspect_argv(tool: &str, image: &str) -> Vec<String> {
    match tool {
        "skopeo" => vec![
            "inspect".to_string(),
            "--format".to_string(),
            "json".to_string(),
            format!("docker://{image}"),
        ],
        "crane" => vec!["config".to_string(), image.to_string()],
        "docker" => vec![
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{json .Config.Labels}}".to_string(),
            image.to_string(),
        ],
        other => unreachable!("unknown label tool {other:?}"),
    }
}

/// Normalize one tool's stdout into a label map. Each [`LABEL_TOOLS`] member
/// has its own answer shape, so the tool name picks the reader — a wrong
/// shape is *that tool's* failure (the cascade falls through), never a
/// misreading of one tool's document as another's label map:
///
/// - `skopeo` (`inspect --format json`): an inspect document with `Labels`
///   at the top level — `null` when the image carries no labels;
/// - `crane` (`config`): the OCI config blob, labels at `config.Labels`
///   (absent when the image carries none);
/// - `docker` (`image inspect --format {{json .Config.Labels}}`): the whole
///   answer *is* the label object — `null` when unlabeled.
///
/// Label values must all be strings (OCI label values are); anything else is
/// an error naming the shape problem, never a partial map.
fn normalize_labels(tool: &str, text: &str) -> Result<BTreeMap<String, String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match tool {
        "skopeo" => {
            let document = value.as_object().ok_or("not a JSON object")?;
            label_object(document.get("Labels").unwrap_or(&serde_json::Value::Null))
        }
        "crane" => {
            let document = value.as_object().ok_or("not a JSON object")?;
            let config = document
                .get("config")
                .ok_or("no config object — not an OCI config blob")?;
            label_object(config.get("Labels").unwrap_or(&serde_json::Value::Null))
        }
        "docker" => label_object(&value),
        other => Err(format!(
            "unknown label tool {other:?} — no known answer shape"
        )),
    }
}

/// Read one JSON value as a label map: null is an empty (unlabeled) map, an
/// object must be all-string-valued, anything else is a shape error.
fn label_object(value: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let Some(object) = value.as_object() else {
        // `null` — the standard docker answer for an unlabeled image — is an
        // empty map, which classifies as unlabeled downstream.
        return if value.is_null() {
            Ok(BTreeMap::new())
        } else {
            Err("labels value is neither an object nor null".to_string())
        };
    };
    let mut labels = BTreeMap::new();
    for (key, val) in object {
        let Some(text) = val.as_str() else {
            return Err(format!("label {key:?} is not a string value"));
        };
        labels.insert(key.clone(), text.to_string());
    }
    Ok(labels)
}

/// Tool stderr for a failure reason: informative, not a wall.
fn shorten(text: &str) -> String {
    let trimmed = text.trim();
    const LIMIT: usize = 200;
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed.chars().take(LIMIT).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // The fake inspector — the injectable trait under test
    // ------------------------------------------------------------------

    /// A fake `ImageInspector` carrying one canned answer. No network, no
    /// registry tool: the production spawn path is never entered.
    struct Fake {
        seen: std::cell::RefCell<Vec<String>>,
        answer: Result<ImageLabels, LookupFailure>,
    }

    impl Fake {
        fn ok(labels: &[(&str, &str)]) -> Self {
            Fake {
                seen: std::cell::RefCell::new(Vec::new()),
                answer: Ok(ImageLabels::from_map(
                    labels
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                )),
            }
        }

        fn failing(failure: LookupFailure) -> Self {
            Fake {
                seen: std::cell::RefCell::new(Vec::new()),
                answer: Err(failure),
            }
        }
    }

    impl ImageInspector for Fake {
        fn labels(&self, image: &str) -> Result<ImageLabels, LookupFailure> {
            self.seen.borrow_mut().push(image.to_string());
            self.answer.clone()
        }
    }

    const BUILDER: &str = "registry.test/gantry-builder:1.98.1";

    // ------------------------------------------------------------------
    // capability_of — labeled / unlabeled / lookup-error, per acceptance
    // ------------------------------------------------------------------

    #[test]
    fn labeled_image_yields_the_parsed_capability() {
        let fake = Fake::ok(&[
            (
                TOOLCHAIN_LABEL_KEY,
                r#"{"channel":"stable","pin":"1.98.1","features":["sccache"]}"#,
            ),
            ("maintainer", "ops@example.com"),
        ]);
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Known { label } = capability else {
            panic!("a labeled image is Known, got: {capability:?}")
        };
        assert_eq!(label.channel, "stable");
        assert_eq!(label.pin.as_deref(), Some("1.98.1"));
        assert!(label.supports_feature("sccache"));
        assert_eq!(fake.seen.into_inner(), vec![BUILDER.to_string()]);
    }

    #[test]
    fn minimal_payload_still_known() {
        let fake = Fake::ok(&[(TOOLCHAIN_LABEL_KEY, r#"{"channel":"1.98.1"}"#)]);
        let ImageCapability::Known { label } = capability_of(&fake, BUILDER) else {
            panic!("a channel-only payload is Known")
        };
        assert_eq!(label.channel, "1.98.1");
        assert_eq!(label.pin, None);
    }

    #[test]
    fn unlabeled_image_is_unknown_with_a_reason() {
        let fake = Fake::ok(&[("maintainer", "ops@example.com")]);
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Unknown { reason } = &capability else {
            panic!("an unlabeled image is Unknown, got: {capability:?}")
        };
        assert!(
            reason.contains(GANTRY_LABEL_PREFIX),
            "the unlabeled reason names the label namespace, got: {reason:?}"
        );
    }

    #[test]
    fn gantry_labels_without_the_toolchain_key_are_unknown() {
        let fake = Fake::ok(&[("org.gantry.capabilities", "sccache")]);
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Unknown { reason } = &capability else {
            panic!("org.gantry labels without the toolchain key are Unknown, got: {capability:?}")
        };
        assert!(
            reason.contains(TOOLCHAIN_LABEL_KEY),
            "the reason names the missing key, got: {reason:?}"
        );
    }

    #[test]
    fn unparseable_payload_is_unknown_not_a_guess() {
        let fake = Fake::ok(&[(TOOLCHAIN_LABEL_KEY, r#""1.98.1""#)]);
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Unknown { reason } = &capability else {
            panic!("a malformed payload is Unknown, got: {capability:?}")
        };
        assert!(
            reason.contains("does not parse"),
            "the reason names the parse failure, got: {reason:?}"
        );
    }

    #[test]
    fn missing_tooling_maps_to_unknown_with_the_tool_list() {
        let fake = Fake::failing(LookupFailure::MissingTooling {
            tried: LABEL_TOOLS.iter().map(|t| t.to_string()).collect(),
        });
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Unknown { reason } = &capability else {
            panic!("missing tooling is Unknown, got: {capability:?}")
        };
        assert!(
            reason.contains("skopeo") && reason.contains("docker"),
            "the reason names what was looked for, got: {reason:?}"
        );
    }

    #[test]
    fn lookup_error_maps_to_unknown_with_the_detail() {
        let fake = Fake::failing(LookupFailure::LookupError {
            detail: "skopeo exited 1: unauthorized".to_string(),
        });
        let capability = capability_of(&fake, BUILDER);
        let ImageCapability::Unknown { reason } = &capability else {
            panic!("a lookup error is Unknown, got: {capability:?}")
        };
        assert!(
            reason.contains("unauthorized"),
            "the reason carries the tool's complaint, got: {reason:?}"
        );
    }

    #[test]
    fn every_unknown_carries_a_non_empty_reason() {
        let answers = vec![
            Fake::ok(&[]).answer,
            Fake::ok(&[("org.gantry.other", "x")]).answer,
            Fake::ok(&[(TOOLCHAIN_LABEL_KEY, "not json")]).answer,
            Fake::failing(LookupFailure::MissingTooling { tried: vec![] }).answer,
            Fake::failing(LookupFailure::LookupError {
                detail: String::new(),
            })
            .answer,
        ];
        for answer in answers {
            let inspector = Fake {
                seen: Default::default(),
                answer,
            };
            let ImageCapability::Unknown { reason } = capability_of(&inspector, BUILDER) else {
                panic!("every failure posture is Unknown")
            };
            assert!(!reason.is_empty(), "an Unknown reason is never empty");
        }
    }

    #[test]
    fn registry_inspector_satisfies_the_trait() {
        // The production inspector is interchangeable with the fake: a
        // `dyn ImageInspector` binding is all the injectability contract
        // needs (never called — that would spawn real tools).
        let _inspector: Box<dyn ImageInspector> = Box::new(RegistryInspector);
    }

    // ------------------------------------------------------------------
    // inspect_via — the tool cascade, with fakes
    // ------------------------------------------------------------------

    /// Run `inspect_via` with per-tool scripted answers.
    fn scripted(
        present: &[&str],
        answers: &[(&str, Result<&str, &str>)],
    ) -> Result<ImageLabels, LookupFailure> {
        let available = |tool: &str| present.contains(&tool);
        let run = |tool: &str, _argv: &[String]| -> Result<String, String> {
            answers
                .iter()
                .find(|(name, _)| *name == tool)
                .map(|(_, result)| (*result).map(str::to_string).map_err(str::to_string))
                .unwrap_or_else(|| panic!("{tool} was not expected to run"))
        };
        inspect_via(&available, &run, BUILDER)
    }

    const SKOPEO_ANSWER: &str =
        r#"{"Name":"gantry-builder","Labels":{"org.gantry.toolchain":"{\"channel\":\"1.98.1\"}"}}"#;
    const CRANE_ANSWER: &str = r#"{"architecture":"amd64","config":{"Labels":{"org.gantry.toolchain":"{\"channel\":\"1.98.1\"}"}}}"#;

    #[test]
    fn no_tools_on_path_is_missing_tooling_naming_the_cascade() {
        let failure = scripted(&[], &[]).expect_err("nothing available");
        let LookupFailure::MissingTooling { tried } = failure else {
            panic!("an empty PATH section is MissingTooling, got: {failure:?}")
        };
        assert_eq!(
            tried.iter().map(String::as_str).collect::<Vec<_>>(),
            LABEL_TOOLS.to_vec(),
            "every tool in the cascade was looked for, in order"
        );
    }

    #[test]
    fn first_answer_wins_and_later_tools_never_run() {
        let labels = scripted(
            &["skopeo", "crane", "docker"],
            &[("skopeo", Ok(SKOPEO_ANSWER)), ("crane", Ok(CRANE_ANSWER))],
        )
        .expect("skopeo answers");
        assert_eq!(
            labels.get(TOOLCHAIN_LABEL_KEY),
            Some(r#"{"channel":"1.98.1"}"#),
            "skopeo's answer is the lookup result"
        );
    }

    #[test]
    fn cascade_falls_through_a_failing_tool() {
        let labels = scripted(
            &["skopeo", "crane"],
            &[
                ("skopeo", Err("Cannot connect to the Docker daemon")),
                ("crane", Ok(CRANE_ANSWER)),
            ],
        )
        .expect("crane answers after skopeo fails");
        assert!(labels.gantry_labeled());
    }

    #[test]
    fn every_tool_failing_collects_each_failure() {
        let failure = scripted(
            &["skopeo", "docker"],
            &[
                ("skopeo", Err("unauthorized")),
                ("docker", Err("daemon down")),
            ],
        )
        .expect_err("no tool answered");
        let LookupFailure::LookupError { detail } = failure else {
            panic!("all-tools-failed is a LookupError, got: {failure:?}")
        };
        assert!(
            detail.contains("skopeo unauthorized") && detail.contains("docker daemon down"),
            "each tool's failure rides the detail, got: {detail:?}"
        );
        assert!(detail.contains("crane"), "the absent tool is named too");
    }

    #[test]
    fn unreadable_output_from_the_first_tool_falls_through() {
        // A tool that answers garbage has not answered: the cascade tries
        // the next tool before giving up.
        let labels = scripted(
            &["skopeo", "crane"],
            &[("skopeo", Ok("not json")), ("crane", Ok(CRANE_ANSWER))],
        )
        .expect("crane answers after skopeo prints garbage");
        assert!(labels.gantry_labeled());
    }

    #[test]
    fn garbage_from_every_tool_is_a_lookup_error() {
        let failure = scripted(
            &["skopeo", "crane", "docker"],
            &[
                ("skopeo", Ok("<html>502</html>")),
                ("crane", Ok("null")),
                ("docker", Ok("[]")),
            ],
        )
        .expect_err("no tool produced a label map");
        let LookupFailure::LookupError { detail } = failure else {
            panic!("garbage everywhere is a LookupError, got: {failure:?}")
        };
        assert!(
            detail.contains("skopeo") && detail.contains("not JSON"),
            "the first tool's shape error is in the detail, got: {detail:?}"
        );
    }

    #[test]
    fn argv_pins_each_tool_shape() {
        assert_eq!(
            inspect_argv("skopeo", "reg.example.com/b:1"),
            vec![
                "inspect",
                "--format",
                "json",
                "docker://reg.example.com/b:1"
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            inspect_argv("crane", "reg.example.com/b:1"),
            vec!["config", "reg.example.com/b:1"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            inspect_argv("docker", "reg.example.com/b:1"),
            vec![
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                "reg.example.com/b:1"
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // normalize_labels — the three answer shapes
    // ------------------------------------------------------------------

    #[test]
    fn skopeo_document_reads_labels_at_the_top_level() {
        let labels =
            ImageLabels::from_map(normalize_labels("skopeo", SKOPEO_ANSWER).expect("skopeo shape"));
        assert_eq!(
            labels.get(TOOLCHAIN_LABEL_KEY),
            Some(r#"{"channel":"1.98.1"}"#)
        );
    }

    #[test]
    fn skopeo_null_labels_is_an_empty_map() {
        let labels = ImageLabels::from_map(
            normalize_labels("skopeo", r#"{"Name":"b","Labels":null}"#).expect("null labels"),
        );
        assert!(labels.as_map().is_empty(), "null Labels is unlabeled");
    }

    #[test]
    fn oci_config_blob_reads_labels_at_config_labels() {
        let labels =
            ImageLabels::from_map(normalize_labels("crane", CRANE_ANSWER).expect("OCI blob shape"));
        assert_eq!(
            labels.get(TOOLCHAIN_LABEL_KEY),
            Some(r#"{"channel":"1.98.1"}"#)
        );
    }

    #[test]
    fn crane_labels_may_be_absent_from_the_config_blob() {
        let labels = ImageLabels::from_map(
            normalize_labels("crane", r#"{"architecture":"amd64","config":{}}"#)
                .expect("config blob without Labels"),
        );
        assert!(
            labels.as_map().is_empty(),
            "an absent config.Labels is unlabeled, not an error"
        );
    }

    #[test]
    fn docker_answer_is_the_label_object_itself() {
        let labels = ImageLabels::from_map(
            normalize_labels(
                "docker",
                r#"{"org.gantry.toolchain":"{\"channel\":\"stable\"}"}"#,
            )
            .expect("docker shape"),
        );
        assert!(labels.gantry_labeled());
    }

    #[test]
    fn docker_null_answer_is_an_empty_map() {
        let labels = ImageLabels::from_map(
            normalize_labels("docker", "null").expect("docker's unlabeled answer"),
        );
        assert!(
            labels.as_map().is_empty(),
            "docker prints null for an image with no labels"
        );
    }

    #[test]
    fn non_string_label_values_and_garbage_are_shape_errors() {
        assert!(normalize_labels("skopeo", "not json at all").is_err());
        assert!(normalize_labels("skopeo", "[]").is_err());
        assert!(normalize_labels("skopeo", r#"{"Labels":{"a":1}}"#).is_err());
        assert!(
            normalize_labels("skopeo", r#"{"Labels":"stable"}"#).is_err(),
            "a string where the label object belongs is a shape error"
        );
        // `null` is only docker's unlabeled answer: from skopeo or crane it
        // is a document where the label map belongs, i.e. garbage.
        assert!(normalize_labels("skopeo", "null").is_err());
        assert!(normalize_labels("crane", "null").is_err());
    }

    // ------------------------------------------------------------------
    // ImageLabels accessors
    // ------------------------------------------------------------------

    #[test]
    fn gantry_labeled_keys_off_the_prefix() {
        let gantry =
            ImageLabels::from_map([(TOOLCHAIN_LABEL_KEY.to_string(), "{}".to_string())].into());
        assert!(gantry.gantry_labeled());
        let plain = ImageLabels::from_map([("maintainer".to_string(), "x".to_string())].into());
        assert!(!plain.gantry_labeled());
        let empty = ImageLabels::from_map(Default::default());
        assert!(!empty.gantry_labeled());
    }

    #[test]
    fn lookup_failure_displays_a_usable_reason() {
        let missing = LookupFailure::MissingTooling {
            tried: vec!["skopeo".to_string(), "crane".to_string()],
        };
        assert_eq!(
            missing.to_string(),
            "no read-only registry tool on PATH (tried skopeo, crane)"
        );
        let failed = LookupFailure::LookupError {
            detail: "skopeo exited 1: no such image".to_string(),
        };
        assert_eq!(
            failed.to_string(),
            "label lookup failed: skopeo exited 1: no such image"
        );
    }
}
