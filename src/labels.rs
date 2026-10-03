// gantry — image capability label schema for parity preflight (plan Component 5).
//
// Builder images carry capability labels under the `org.gantry.toolchain`
// key; before submission gantry compares the local rust-toolchain.toml and
// the requested features against them — "a mismatch is a loud local fallback
// (warn-only for unlabeled images), because a verdict from the wrong
// toolchain is a wrong answer delivered confidently" (plan §Component 5,
// argo backend; ideas-ledger finalist 4, adopted 2026-07-22).
//
// This module is the library-level data model only: the label key constant,
// the parsed label struct, and the strict parser that turns the raw label
// value into that struct. Nothing here touches a backend or the submit path —
// the preflight that consumes these types is a later child of the same split.
//
// The label value is a JSON object (compact, one string — OCI label values
// are plain strings). JSON rather than a bespoke `k=v;k=v` microsyntax
// because the shape has already grown once (channel → pin → feature set) and
// serde gives the strict, typed parse this schema promises without hand-rolled
// splitting; every failure lands in a typed [`LabelParseError`], never a
// stringly-typed panic at a call site.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The OCI label key a builder image uses to publish its toolchain
/// capabilities (plan §Component 5: `org.gantry.toolchain=…`).
///
/// Reverse-domain form, matching the `org.opencontainers.image.*` label
/// convention; the value is a JSON-encoded [`CapabilityLabel`].
pub const TOOLCHAIN_LABEL_KEY: &str = "org.gantry.toolchain";

/// A parsed `org.gantry.toolchain` capability label: what toolchain the
/// builder image runs and which features it supports.
///
/// `channel` is the rustup channel the image's default toolchain resolves to
/// (`stable`, `nightly`, or a concrete version such as `1.98.1` — exactly the
/// spellings a rust-toolchain.toml `channel` accepts). `pin` is the optional
/// exact refinement the image guarantees beyond a moving channel name (a
/// dated nightly or a precise stable version); absent when the image tracks
/// the channel un-pinned. `features` is the supported feature set — an open
/// set of capability tags (`sccache` is the plan's first example — the
/// reference Argo template marks it optional), so a feature coined by a newer
/// image must parse here rather than poison the label.
///
/// Unknown JSON fields are ignored on parse (additive evolution: an older
/// gantry reading a newer label must not refuse the image — the same
/// consumer rule verdict.json already fixed in plan §"Versioning &
/// compatibility"). Required fields are strict: no channel, no capability
/// claim worth preflighting against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityLabel {
    /// The rustup channel the image's toolchain resolves to.
    pub channel: String,
    /// Optional exact pin (dated nightly, precise version) the image
    /// guarantees; absent when tracking the channel un-pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// The supported feature set (`sccache`, …) — open, order-independent.
    #[serde(default)]
    pub features: BTreeSet<String>,
}

impl CapabilityLabel {
    /// Strictly parse a raw label value into a [`CapabilityLabel`].
    ///
    /// The failure ladder is typed, in the order a reader hits it:
    /// [`LabelParseError::Empty`] (label present, value empty — the "image
    /// tried but shipped nothing" case), [`LabelParseError::InvalidJson`]
    /// (not JSON at all), [`LabelParseError::NotAnObject`] (valid JSON, wrong
    /// shape — a bare string is a label saying `1.98.1`, not a capability
    /// claim), [`LabelParseError::MissingChannel`] (an object that names no
    /// channel — absent, null, or empty), and [`LabelParseError::Malformed`]
    /// (right shape, wrong-typed field). Unknown fields never fail.
    pub fn parse(value: &str) -> Result<Self, LabelParseError> {
        if value.trim().is_empty() {
            return Err(LabelParseError::Empty);
        }
        let parsed: serde_json::Value =
            serde_json::from_str(value).map_err(|e| LabelParseError::InvalidJson(e.to_string()))?;
        if !parsed.is_object() {
            return Err(LabelParseError::NotAnObject);
        }
        match parsed.get("channel") {
            None | Some(serde_json::Value::Null) => {
                return Err(LabelParseError::MissingChannel);
            }
            Some(serde_json::Value::String(s)) if s.is_empty() => {
                return Err(LabelParseError::MissingChannel);
            }
            _ => {}
        }
        serde_json::from_value(parsed).map_err(|e| LabelParseError::Malformed(e.to_string()))
    }

    /// Render this label as the label value [`CapabilityLabel::parse`]
    /// accepts — the write half of the round-trip.
    ///
    /// Compact JSON (labels travel inside image metadata, not documents);
    /// serialization of this plain string/set struct is infallible, so the
    /// result is returned directly.
    pub fn to_label_value(&self) -> String {
        serde_json::to_string(self)
            .expect("CapabilityLabel serialization is infallible: string, Option<String>, and set-of-string fields only")
    }

    /// Whether the image claims support for a feature (plan: "requested
    /// features" are compared against the label at preflight).
    pub fn supports_feature(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }
}

/// Why a `org.gantry.toolchain` label value could not be parsed.
///
/// Typed, not a string: the preflight's fallback reason names the shape
/// problem precisely ("a mismatch is a loud local fallback" — the loudness
/// needs a precise noun), and tests match variants, never substrings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelParseError {
    /// The label is present but its value is empty (or whitespace only).
    Empty,
    /// The value is not valid JSON at all.
    InvalidJson(String),
    /// The value is valid JSON but not an object.
    NotAnObject,
    /// The object names no usable channel (absent, null, or empty string).
    MissingChannel,
    /// The object is the right shape but a field has the wrong type.
    Malformed(String),
}

impl fmt::Display for LabelParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabelParseError::Empty => {
                write!(f, "{TOOLCHAIN_LABEL_KEY} label is present but empty")
            }
            LabelParseError::InvalidJson(msg) => {
                write!(f, "{TOOLCHAIN_LABEL_KEY} label is not valid JSON: {msg}")
            }
            LabelParseError::NotAnObject => write!(
                f,
                "{TOOLCHAIN_LABEL_KEY} label is valid JSON but not an object"
            ),
            LabelParseError::MissingChannel => {
                write!(f, "{TOOLCHAIN_LABEL_KEY} label names no channel")
            }
            LabelParseError::Malformed(msg) => {
                write!(f, "{TOOLCHAIN_LABEL_KEY} label is malformed: {msg}")
            }
        }
    }
}

impl std::error::Error for LabelParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    // --- valid parses ---------------------------------------------------

    #[test]
    fn parse_valid_full_payload() {
        let label =
            CapabilityLabel::parse(r#"{"channel":"stable","pin":"1.98.1","features":["sccache"]}"#)
                .expect("full payload parses");
        assert_eq!(label.channel, "stable");
        assert_eq!(label.pin.as_deref(), Some("1.98.1"));
        assert!(label.supports_feature("sccache"));
    }

    #[test]
    fn parse_channel_only_defaults_pin_and_features() {
        let label =
            CapabilityLabel::parse(r#"{"channel":"1.98.1"}"#).expect("channel-only payload parses");
        assert_eq!(label.channel, "1.98.1");
        assert_eq!(label.pin, None);
        assert!(label.features.is_empty());
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        // Additive evolution: a field coined by a newer image must not
        // poison the label for this parser (verdict.json's consumer rule).
        let label = CapabilityLabel::parse(
            r#"{"channel":"stable","rustup_version":"1.28.2","future_thing":42}"#,
        )
        .expect("unknown fields are ignored");
        assert_eq!(label.channel, "stable");
    }

    #[test]
    fn features_form_a_set() {
        // Duplicate entries dedupe, and membership is order-independent.
        let label = CapabilityLabel::parse(
            r#"{"channel":"stable","features":["sccache","docker","sccache"]}"#,
        )
        .expect("features array parses");
        assert_eq!(label.features.len(), 2);
        assert!(label.supports_feature("docker"));
    }

    // --- round-trip -----------------------------------------------------

    #[test]
    fn round_trip_full_label() {
        let mut features = BTreeSet::new();
        features.insert("sccache".to_string());
        features.insert("kubeconfig".to_string());
        let label = CapabilityLabel {
            channel: "nightly".to_string(),
            pin: Some("2026-09-01".to_string()),
            features,
        };
        assert_eq!(
            CapabilityLabel::parse(&label.to_label_value()).unwrap(),
            label
        );
    }

    #[test]
    fn round_trip_minimal_label() {
        let label = CapabilityLabel {
            channel: "stable".to_string(),
            pin: None,
            features: BTreeSet::new(),
        };
        assert_eq!(
            CapabilityLabel::parse(&label.to_label_value()).unwrap(),
            label
        );
    }

    // --- malformed payloads ---------------------------------------------

    #[test]
    fn parse_empty_value_is_typed_error() {
        assert_eq!(CapabilityLabel::parse(""), Err(LabelParseError::Empty));
        assert_eq!(CapabilityLabel::parse("   \n"), Err(LabelParseError::Empty));
    }

    #[test]
    fn parse_non_json_is_invalid_json() {
        let err = CapabilityLabel::parse("stable with rustfmt").expect_err("prose is not JSON");
        assert!(matches!(err, LabelParseError::InvalidJson(_)));
    }

    #[test]
    fn parse_bare_json_string_is_not_an_object() {
        // The tempting legacy shorthand — a label that just says `1.98.1` —
        // is a shape violation, not a channel: strict means it fails loudly.
        assert_eq!(
            CapabilityLabel::parse(r#""1.98.1""#),
            Err(LabelParseError::NotAnObject)
        );
        assert_eq!(
            CapabilityLabel::parse("[]"),
            Err(LabelParseError::NotAnObject)
        );
    }

    #[test]
    fn parse_absent_channel_is_missing_channel() {
        assert_eq!(
            CapabilityLabel::parse("{}"),
            Err(LabelParseError::MissingChannel)
        );
        assert_eq!(
            CapabilityLabel::parse(r#"{"pin":"1.98.1"}"#),
            Err(LabelParseError::MissingChannel)
        );
    }

    #[test]
    fn parse_null_or_empty_channel_is_missing_channel() {
        assert_eq!(
            CapabilityLabel::parse(r#"{"channel":null}"#),
            Err(LabelParseError::MissingChannel)
        );
        assert_eq!(
            CapabilityLabel::parse(r#"{"channel":""}"#),
            Err(LabelParseError::MissingChannel)
        );
    }

    #[test]
    fn parse_wrong_typed_field_is_malformed() {
        let err = CapabilityLabel::parse(r#"{"channel":"stable","features":"sccache"}"#)
            .expect_err("a bare string is not a feature set");
        assert!(matches!(err, LabelParseError::Malformed(_)));
    }

    #[test]
    fn parse_wrong_typed_channel_is_malformed_not_missing() {
        let err =
            CapabilityLabel::parse(r#"{"channel":42}"#).expect_err("a number is not a channel");
        assert!(matches!(err, LabelParseError::Malformed(_)));
    }

    // --- constants and helpers ------------------------------------------

    #[test]
    fn label_key_is_the_reverse_domain_form() {
        assert_eq!(TOOLCHAIN_LABEL_KEY, "org.gantry.toolchain");
    }

    #[test]
    fn supports_feature_is_false_for_unclaimed_features() {
        let label =
            CapabilityLabel::parse(r#"{"channel":"stable","features":["sccache"]}"#).unwrap();
        assert!(label.supports_feature("sccache"));
        assert!(!label.supports_feature("docker"));
    }

    #[test]
    fn errors_are_std_errors_with_display() {
        let err: Box<dyn std::error::Error> = Box::new(CapabilityLabel::parse("").unwrap_err());
        assert!(!err.to_string().is_empty());
    }
}
