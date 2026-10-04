// gantry — builder-image toolchain parity (plan Component 5, argo bullet).
//
// Parity preflight, plan Component 5 ("Parity preflight: builder images carry
// capability labels (`org.gantry.toolchain=…`); before submission gantry
// compares local rust-toolchain.toml and requested features against them — a
// mismatch is a loud local fallback (warn-only for unlabeled images), because
// a verdict from the wrong toolchain is a wrong answer delivered
// confidently."
//
// This module is the pure half of that preflight: the payload format, both
// parsers, and the comparison. It deliberately knows nothing about backends,
// config, or submission — wiring the verdict into the submit path is the
// caller's job. Keeping it pure keeps the contract unit-testable against the
// full matrix (exact match, mismatch, unlabeled payload, missing local pin)
// without a cluster, an image, or a repo on disk.
//
// The payload format is the label's wire contract; `docs/notes/
// toolchain-label-payload.md` is its human-readable spec, mirroring how
// `src/verdict.rs` and `docs/notes/verdict-json-contract.md` pair up for
// verdict.json.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The OCI label under which a builder image declares the Rust toolchain it
/// provides (plan Component 5, argo bullet). Single definition site — the
/// submit-path preflight reads the image's label map through this name.
pub const TOOLCHAIN_LABEL: &str = "org.gantry.toolchain";

/// The payload schema version this module parses and emits. A payload
/// declaring any other version is not interpretable by this build and parses
/// to `None` — never silently read as v1 (the DD-10 posture verdict.json
/// pins: additive field evolution is free, a new schema version claims a
/// contract this parser does not know).
pub const PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// The image-side half of the comparison: the toolchain the label declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageToolchain {
    /// The rustup toolchain identity the image builds with, e.g. `"stable"`
    /// or `"1.83.0"`.
    pub channel: String,
}

/// The local-side half: the `[toolchain]` pin from the repo's
/// `rust-toolchain.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolchain {
    /// The pinned rustup toolchain identity, e.g. `"stable"` or `"1.83.0"`.
    pub channel: String,
}

/// The outcome of comparing a builder image's `org.gantry.toolchain` label
/// against the repo's local toolchain pin.
///
/// The three variants map directly onto the plan's preflight posture:
/// [`Mismatch`](ToolchainParity::Mismatch) is the loud local fallback, and
/// [`Unlabeled`](ToolchainParity::Unlabeled) is the warn-only path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolchainParity {
    /// The image's toolchain matches the local pin — or no pin exists to
    /// contradict, so parity cannot fail.
    Compatible,
    /// The image builds with a different toolchain than the repo pins.
    /// `expected` is the local pin (what the repo asks the image to provide),
    /// `found` is what the label declares the image actually provides.
    Mismatch {
        /// The local `rust-toolchain.toml` channel.
        expected: String,
        /// The channel the image's label declares.
        found: String,
    },
    /// The label carried no interpretable payload — absent, malformed, or a
    /// schema version this build does not speak. Parity cannot be asserted
    /// either way, so this is the plan's warn-only case, never a refusal.
    Unlabeled,
}

/// The canonical v1 label payload for `channel` — the serializer side of the
/// contract image builds use to label themselves. Round-trips through
/// [`parse_label_payload`].
pub fn label_payload(channel: &str) -> String {
    #[derive(Serialize)]
    struct PayloadV1<'a> {
        schema_version: u32,
        channel: &'a str,
    }
    serde_json::to_string(&PayloadV1 {
        schema_version: PAYLOAD_SCHEMA_VERSION,
        channel,
    })
    .expect("serializing a two-field struct cannot fail")
}

#[derive(Deserialize)]
struct PayloadV1 {
    schema_version: u32,
    channel: String,
}

/// Parse a builder image's `org.gantry.toolchain` label value into the
/// toolchain it declares.
///
/// `None` means the payload is not interpretable at this schema version —
/// the label is absent, the value is not the documented JSON object, or it
/// declares a `schema_version` other than [`PAYLOAD_SCHEMA_VERSION`]. All
/// three leave parity unassertable; unknown *fields* inside a v1 payload are
/// ignored (the verdict.json compatibility rule), unknown *versions* are not
/// read as v1.
pub fn parse_label_payload(label: Option<&str>) -> Option<ImageToolchain> {
    let raw = label?;
    let payload: PayloadV1 = serde_json::from_str(raw).ok()?;
    (payload.schema_version == PAYLOAD_SCHEMA_VERSION).then_some(ImageToolchain {
        channel: payload.channel,
    })
}

#[derive(Deserialize)]
struct ToolchainFile {
    toolchain: Option<ToolchainSection>,
}

#[derive(Deserialize)]
struct ToolchainSection {
    channel: Option<String>,
}

/// Parse the text of a `rust-toolchain.toml` into the pin it declares.
///
/// `None` means the file declares no `[toolchain].channel` — absent section,
/// absent key, or text that does not parse as that shape at all. Sibling
/// keys (`components`, `targets`, `profile`) are ignored here: this module
/// compares toolchain identity only; feature comparison is the preflight's
/// later half (plan: "and requested features").
pub fn parse_local_toolchain(text: &str) -> Option<LocalToolchain> {
    let file: ToolchainFile = toml::from_str(text).ok()?;
    let channel = file.toolchain?.channel?;
    Some(LocalToolchain { channel })
}

/// Read the repo root's `rust-toolchain.toml` into the pin it declares.
/// `None` when the file is missing, unreadable, or declares no channel —
/// a repo with no pin has no toolchain expectation for an image to violate.
pub fn read_local_toolchain(repo_root: &Path) -> Option<LocalToolchain> {
    let text = std::fs::read_to_string(repo_root.join("rust-toolchain.toml")).ok()?;
    parse_local_toolchain(&text)
}

/// Compare a builder image's label value against the local pin.
///
/// `label` is the image's `org.gantry.toolchain` label value (`None` when
/// the image carries no such label); `local` is [`read_local_toolchain`]'s
/// result. The verdict:
///
/// - an uninterpretable payload (absent, malformed, future version) is
///   [`ToolchainParity::Unlabeled`] — the plan's warn-only case, whatever
///   the local side says;
/// - a readable payload with no local pin is
///   [`ToolchainParity::Compatible`] — a repo that pins nothing cannot
///   mismatch;
/// - otherwise the channels decide, by exact string equality. Rustup
///   identities are compared verbatim: `"1.83"` vs `"1.83.0"` is a
///   [`ToolchainParity::Mismatch`], not a fuzzy match. The asymmetry
///   justifies the conservatism — a false Mismatch costs one local
///   fallback run, a false Compatible costs a confidently wrong verdict.
pub fn compare(label: Option<&str>, local: Option<&LocalToolchain>) -> ToolchainParity {
    let Some(image) = parse_label_payload(label) else {
        return ToolchainParity::Unlabeled;
    };
    let Some(local) = local else {
        return ToolchainParity::Compatible;
    };
    if image.channel == local.channel {
        ToolchainParity::Compatible
    } else {
        ToolchainParity::Mismatch {
            expected: local.channel.clone(),
            found: image.channel,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(channel: &str) -> LocalToolchain {
        LocalToolchain {
            channel: channel.to_string(),
        }
    }

    // --- the comparison matrix (bead acceptance) ---

    #[test]
    fn exact_match_is_compatible() {
        let parity = compare(Some(&label_payload("stable")), Some(&local("stable")));
        assert_eq!(parity, ToolchainParity::Compatible);
    }

    #[test]
    fn mismatch_names_expected_and_found() {
        let parity = compare(Some(&label_payload("stable")), Some(&local("1.83.0")));
        assert_eq!(
            parity,
            ToolchainParity::Mismatch {
                expected: "1.83.0".to_string(),
                found: "stable".to_string(),
            }
        );
    }

    #[test]
    fn unlabeled_label_is_unlabeled_even_with_a_local_pin() {
        let parity = compare(None, Some(&local("stable")));
        assert_eq!(parity, ToolchainParity::Unlabeled);
    }

    #[test]
    fn missing_local_pin_is_compatible() {
        // No rust-toolchain.toml: the repo pins nothing, so no label can
        // violate it — matching and mismatching labels alike.
        assert_eq!(
            compare(Some(&label_payload("stable")), None),
            ToolchainParity::Compatible
        );
        assert_eq!(
            compare(Some(&label_payload("nightly")), None),
            ToolchainParity::Compatible
        );
    }

    #[test]
    fn unlabeled_label_with_no_local_pin_is_still_unlabeled() {
        // Label absence is about the image, not the repo — it stays the
        // warn-only case even when there is no pin to check against.
        assert_eq!(compare(None, None), ToolchainParity::Unlabeled);
    }

    // --- the label payload format ---

    #[test]
    fn label_payload_is_the_canonical_v1_json() {
        assert_eq!(
            label_payload("stable"),
            r#"{"schema_version":1,"channel":"stable"}"#
        );
    }

    #[test]
    fn label_payload_round_trips_through_the_parser() {
        let parsed = parse_label_payload(Some(&label_payload("1.83.0")));
        assert_eq!(
            parsed,
            Some(ImageToolchain {
                channel: "1.83.0".to_string(),
            })
        );
    }

    #[test]
    fn payload_unknown_fields_are_ignored() {
        // Additive field evolution is free (the verdict.json rule): a v1
        // payload carrying fields this build never heard of still parses.
        let raw = r#"{"schema_version":1,"channel":"stable","components":["clippy"]}"#;
        assert_eq!(
            parse_label_payload(Some(raw)),
            Some(ImageToolchain {
                channel: "stable".to_string(),
            })
        );
    }

    #[test]
    fn unparseable_payload_reads_as_unlabeled() {
        // Warn-only, never a refusal: parity cannot be asserted from a
        // payload that does not parse.
        assert_eq!(
            compare(Some("rust:1.83"), Some(&local("1.83"))),
            ToolchainParity::Unlabeled
        );
    }

    #[test]
    fn future_schema_version_is_not_read_as_v1() {
        // A payload from a schema this build does not speak must not be
        // misread channel-wise as v1 (the DD-10 strictness verdict.json
        // pins). It degrades to the warn-only case.
        let raw = r#"{"schema_version":2,"channel":"stable"}"#;
        assert_eq!(parse_label_payload(Some(raw)), None);
        assert_eq!(
            compare(Some(raw), Some(&local("stable"))),
            ToolchainParity::Unlabeled
        );
    }

    #[test]
    fn payload_without_a_channel_is_uninterpretable() {
        let raw = r#"{"schema_version":1}"#;
        assert_eq!(parse_label_payload(Some(raw)), None);
    }

    #[test]
    fn channel_comparison_is_exact_not_fuzzy() {
        // "1.83" is a rustup shorthand for "latest 1.83.x", but v1 compares
        // identities verbatim — the conservative reading (module docs).
        let parity = compare(Some(&label_payload("1.83")), Some(&local("1.83.0")));
        assert_eq!(
            parity,
            ToolchainParity::Mismatch {
                expected: "1.83.0".to_string(),
                found: "1.83".to_string(),
            }
        );
    }

    // --- the local side ---

    #[test]
    fn local_parse_reads_a_repo_shaped_toolchain_file() {
        // The shape of this repo's own rust-toolchain.toml: comments,
        // channel, and sibling keys this comparison ignores.
        let text = r#"
# Toolchain pin for gantry.
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
"#;
        assert_eq!(
            parse_local_toolchain(text),
            Some(LocalToolchain {
                channel: "stable".to_string(),
            })
        );
    }

    #[test]
    fn local_file_without_a_channel_declares_no_pin() {
        assert_eq!(
            parse_local_toolchain("[toolchain]\ncomponents = []\n"),
            None
        );
        assert_eq!(parse_local_toolchain("# no toolchain section\n"), None);
        assert_eq!(parse_local_toolchain("not toml ["), None);
    }

    #[test]
    fn version_pin_parses_like_a_channel_pin() {
        assert_eq!(
            parse_local_toolchain("[toolchain]\nchannel = \"1.83.0\"\n"),
            Some(LocalToolchain {
                channel: "1.83.0".to_string(),
            })
        );
    }

    #[test]
    fn missing_local_toolchain_file_reads_as_no_pin() {
        // The IO half of the "missing local toolchain file" matrix entry:
        // an empty repo root has no file, so no pin.
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_local_toolchain(dir.path()), None);
    }

    #[test]
    fn read_local_toolchain_reads_the_real_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.83.0\"\n",
        )
        .unwrap();
        assert_eq!(
            read_local_toolchain(dir.path()),
            Some(LocalToolchain {
                channel: "1.83.0".to_string(),
            })
        );
    }
}
