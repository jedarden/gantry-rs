// gantry — the parity preflight decision (plan Component 5, part 3).
//
// "Before submission gantry compares local rust-toolchain.toml and requested
// features against [the builder image's capability labels] — a mismatch is a
// loud local fallback (warn-only for unlabeled images), because a verdict
// from the wrong toolchain is a wrong answer delivered confidently" (plan
// §Component 5, argo backend). This module is that comparison: a pure
// function from the two halves the acquisition beads built — the repo's pin
// ([`crate::toolchain`], part 1) and the image's capability
// ([`crate::capability`], part 2) — to one of three decisions:
//
// - match    → submit, silently;
// - mismatch → refuse: the caller skips the remote run entirely and falls
//              back to local execution, behind a loud `[gantry]` line;
// - unknown  → submit, but only after a loud `[gantry]` warning: an image
//              whose capability could not be established is never a refusal,
//              because the refusal must be *provable* — "never manufacture a
//              mismatch from a guess" (part 2's contract).
//
// The decision owns the message text. Every line it produces starts with
// `[gantry] parity:` — the same `[gantry]-prefixed stderr convention as the
// cap note, the gate attributions, and the timeout line — and the argo
// backend prints the lines verbatim (src/backend/argo.rs), so the loudness
// contract is asserted here once, at the single definition site.
//
// Refusals are deliberately narrow. Only a *known* label can refuse, and
// only on two provable grounds: the image's channel cannot satisfy the
// repo's pin, or the image does not advertise a feature the run requested.
// An unpinned repo has no channel claim to violate; a malformed pin file is
// a repo-side defect rustup would hit locally too, so proceeding is the
// parity-faithful move (warn, and let the same failure surface wherever it
// would have surfaced without gantry).

use std::path::Path;

use crate::capability::ImageCapability;
use crate::labels::CapabilityLabel;
use crate::toolchain::{self, ToolchainRequest, ToolchainTomlError};

/// The stderr prefix every preflight line carries. The `[gantry]` head is the
/// loudness contract ("a mismatch is a loud local fallback"); `parity:` names
/// the subsystem, matching the `[gantry] cap:` / `[gantry] gate:` house style.
pub const PARITY_PREFIX: &str = "[gantry] parity:";

/// What the repo's rust-toolchain.toml asked for, as the preflight sees it.
///
/// The three shapes mirror [`toolchain::read`]'s outcomes one-to-one: no file
/// is an unpinned repo (no channel claim to compare), a parsed file is the
/// pin, and an unreadable or malformed file is a repo-side defect — carried
/// with its reason for the warn line, never flattened into "unpinned" (that
/// would silently downgrade a broken pin file into no claim at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPin {
    /// No rust-toolchain.toml — the repo is unpinned.
    Unpinned,
    /// The file parsed: the channel claim to compare against the image.
    Pinned(ToolchainRequest),
    /// The file exists but is unusable (invalid TOML, no channel, unreadable).
    /// The reason is the warn line's body.
    Malformed { reason: String },
}

impl LocalPin {
    /// Sort a [`toolchain::read`] outcome into the three pin shapes.
    pub fn from_read_result(result: Result<Option<ToolchainRequest>, ToolchainTomlError>) -> Self {
        match result {
            Ok(Some(request)) => LocalPin::Pinned(request),
            Ok(None) => LocalPin::Unpinned,
            Err(why) => LocalPin::Malformed {
                reason: why.to_string(),
            },
        }
    }

    /// Read the repo's pin from disk (`toolchain::read` + [`Self::from_read_result`]).
    pub fn read(path: &Path) -> Self {
        Self::from_read_result(toolchain::read(path))
    }
}

/// The preflight's verdict on one submission.
///
/// The two non-silent variants carry their full stderr line, `[gantry]`
/// prefix included — the caller prints [`Self::loud_line`] verbatim, so the
/// message contract lives here and nowhere else. A refusal additionally
/// carries `detail`, the same explanation *without* the `[gantry] parity:`
/// head: that is the text the backend error channel takes (the decision
/// layer reports it as `[gantry] submit failed: {detail}` — a second
/// `[gantry]`-headed line, with no prefix doubled inside one line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The image provably satisfies the repo's pin and every requested
    /// feature — submit, print nothing.
    Submit,
    /// Parity could not be established — submit, but print `line` first.
    /// The warn-only arm: unlabeled images, failed lookups, unusable pins.
    SubmitWithWarning {
        /// The full stderr line, `[gantry] parity:` prefix included.
        line: String,
    },
    /// Provable toolchain or feature mismatch — the remote run is refused:
    /// no Workflow manifest is built or submitted, and the caller's existing
    /// infra ladder lands the run locally. `line` is the loud stderr
    /// explanation; `detail` is the same reason without the prefix, for the
    /// backend error channel.
    RefuseToLocal {
        /// The full stderr line, `[gantry] parity:` prefix included.
        line: String,
        /// The refusal reason without the `[gantry] parity:` head.
        detail: String,
    },
}

impl Decision {
    /// Build a warn decision: one line, prefix applied.
    fn warn(body: String) -> Self {
        Decision::SubmitWithWarning {
            line: format!("{PARITY_PREFIX} {body}"),
        }
    }

    /// Build a refusal: the loud line and its unprefixed error-channel twin
    /// are two renderings of one reason, built together so they cannot drift.
    fn refuse(body: String) -> Self {
        Decision::RefuseToLocal {
            line: format!("{PARITY_PREFIX} {body}"),
            detail: body,
        }
    }

    /// Whether the submission may proceed (silent match and warn-only alike).
    pub fn is_submit(&self) -> bool {
        matches!(self, Decision::Submit | Decision::SubmitWithWarning { .. })
    }

    /// The stderr line to print before acting on this decision — `None` for a
    /// silent match, the full `[gantry]`-headed line otherwise.
    pub fn loud_line(&self) -> Option<&str> {
        match self {
            Decision::Submit => None,
            Decision::SubmitWithWarning { line } => Some(line),
            Decision::RefuseToLocal { line, .. } => Some(line),
        }
    }
}

/// The preflight decision: compare the repo's pin and the requested features
/// against the builder image's capability claim.
///
/// `image` is the display name the messages use (the configured builder image
/// reference, or a phrase standing in for the template default). It appears in
/// every non-silent message so the operator knows *which* image to relabel or
/// override.
///
/// Order matters and is deliberate: an unknown capability warns immediately
/// (there is nothing to compare against — the warn-only arm); a known label
/// then faces the feature check first (provable regardless of the local
/// pin's state — even a malformed pin file cannot un-prove a missing
/// feature), and finally the channel check, the headline parity property.
pub fn decide(
    pin: &LocalPin,
    capability: &ImageCapability,
    features: &[String],
    image: &str,
) -> Decision {
    let ImageCapability::Known { label } = capability else {
        let ImageCapability::Unknown { reason } = capability else {
            unreachable!("ImageCapability has exactly two variants");
        };
        return Decision::warn(format!(
            "{image}: capability unknown ({reason}) — submitting without a toolchain parity check"
        ));
    };

    if let Some(feature) = features.iter().find(|f| !label.supports_feature(f)) {
        return Decision::refuse(format!(
            "refusing remote run on {image}: the run requests feature {feature:?} but the image \
             does not advertise it — falling back to local execution"
        ));
    }

    match pin {
        LocalPin::Unpinned => Decision::Submit,
        LocalPin::Malformed { reason } => Decision::warn(format!(
            "rust-toolchain.toml is unusable ({reason}) — toolchain parity unverified; \
             submitting anyway"
        )),
        LocalPin::Pinned(request) => {
            if channel_satisfied(&request.channel, label) {
                Decision::Submit
            } else {
                let advertised = match &label.pin {
                    Some(pinned) => format!("{:?} (pinned at {:?})", label.channel, pinned),
                    None => format!("{:?}", label.channel),
                };
                Decision::refuse(format!(
                    "refusing remote run on {image}: repo pins toolchain channel {:?} but the \
                     image advertises {advertised} — falling back to local execution",
                    request.channel
                ))
            }
        }
    }
}

/// Whether the image's toolchain claim satisfies the repo's channel request.
///
/// The repo is satisfied when the image advertises the requested channel
/// verbatim (a moving channel: `stable` tracks stable) *or* when the image's
/// pin is exactly the requested channel (a pinned image delivers the precise
/// toolchain the repo asked for — `{"channel":"stable","pin":"1.98.1"}`
/// satisfies a repo pinned to `1.98.1`, and equally a repo pinned to
/// `stable`, since 1.98.1 *is* a stable). Anything else refuses: a moving
/// `stable` cannot vouch for a repo that pins `1.98.1` (stable moves on), a
/// dated nightly cannot vouch for a repo that wants tracking `nightly`, and
/// a version-pinned image cannot vouch for a repo that wants a different
/// version — each would run the suite on a toolchain the repo did not ask
/// for, which is exactly the confidently-wrong verdict the plan refuses.
fn channel_satisfied(requested: &str, label: &CapabilityLabel) -> bool {
    requested == label.channel || label.pin.as_deref() == Some(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::capability::ImageCapability;

    /// A `ToolchainRequest` pinning `channel`, no components or targets —
    /// the preflight compares channels only (components are rustup's business,
    /// not an image-label capability).
    fn pinned(channel: &str) -> LocalPin {
        LocalPin::Pinned(ToolchainRequest {
            channel: channel.to_string(),
            components: Vec::new(),
            targets: Vec::new(),
        })
    }

    /// An image capability label: `channel`, no pin, the given features.
    fn known(channel: &str, features: &[&str]) -> ImageCapability {
        ImageCapability::Known {
            label: CapabilityLabel {
                channel: channel.to_string(),
                pin: None,
                features: features.iter().map(|f| f.to_string()).collect(),
            },
        }
    }

    /// The same, with an explicit pin refinement.
    fn known_pinned(channel: &str, pin: &str, features: &[&str]) -> ImageCapability {
        ImageCapability::Known {
            label: CapabilityLabel {
                channel: channel.to_string(),
                pin: Some(pin.to_string()),
                features: features.iter().map(|f| f.to_string()).collect(),
            },
        }
    }

    fn unknown(reason: &str) -> ImageCapability {
        ImageCapability::Unknown {
            reason: reason.to_string(),
        }
    }

    fn features(list: &[&str]) -> Vec<String> {
        list.iter().map(|f| f.to_string()).collect()
    }

    const IMAGE: &str = "registry.test/gantry-builder:1";

    // --- match: silent submit -------------------------------------------

    #[test]
    fn equal_channel_submits_silently() {
        let d = decide(&pinned("stable"), &known("stable", &[]), &[], IMAGE);
        assert_eq!(d, Decision::Submit);
        assert!(d.is_submit());
        assert_eq!(d.loud_line(), None, "a match is silent");
    }

    #[test]
    fn pinned_image_satisfies_the_channel_request() {
        // `stable` pinned at 1.98.1 still satisfies a repo asking for stable.
        let d = decide(
            &pinned("stable"),
            &known_pinned("stable", "1.98.1", &[]),
            &[],
            IMAGE,
        );
        assert_eq!(d, Decision::Submit);
    }

    #[test]
    fn pin_refinement_satisfies_the_exact_request() {
        // The mirror: a repo pinned to the exact version the image pins.
        let d = decide(
            &pinned("1.98.1"),
            &known_pinned("stable", "1.98.1", &[]),
            &[],
            IMAGE,
        );
        assert_eq!(d, Decision::Submit);
    }

    #[test]
    fn unpinned_repo_against_a_known_image_submits_silently() {
        // No pin, no channel claim to violate — nothing to refuse.
        let d = decide(&LocalPin::Unpinned, &known("nightly", &[]), &[], IMAGE);
        assert_eq!(d, Decision::Submit);
    }

    #[test]
    fn requested_feature_advertised_submits() {
        let d = decide(
            &pinned("stable"),
            &known("stable", &["sccache"]),
            &features(&["sccache"]),
            IMAGE,
        );
        assert_eq!(d, Decision::Submit);
    }

    #[test]
    fn dated_nightly_pin_matches_a_dated_nightly_image() {
        let d = decide(
            &pinned("nightly-2026-09-01"),
            &known_pinned("nightly", "nightly-2026-09-01", &[]),
            &[],
            IMAGE,
        );
        assert_eq!(d, Decision::Submit);
    }

    // --- mismatch: loud local fallback ----------------------------------

    #[test]
    fn different_channel_refuses() {
        let d = decide(&pinned("stable"), &known("nightly", &[]), &[], IMAGE);
        let Decision::RefuseToLocal { line, detail } = d else {
            panic!("stable vs nightly must refuse, got {d:?}")
        };
        assert!(line.starts_with(PARITY_PREFIX), "loud line: {line:?}");
        assert!(line.contains("[gantry]"), "line: {line:?}");
        assert!(line.contains(IMAGE), "the image is named: {line:?}");
        assert!(
            line.contains("\"stable\"") && line.contains("\"nightly\""),
            "both channels are named: {line:?}"
        );
        assert!(
            line.contains("falling back to local execution"),
            "the fallback is explained: {line:?}"
        );
        // The error-channel text is the same reason without the prefix —
        // the decision layer prints it under its own [gantry] head.
        assert!(!detail.starts_with("[gantry]"), "detail: {detail:?}");
        assert!(line.ends_with(&detail), "line = prefix + detail: {line:?}");
    }

    #[test]
    fn moving_channel_cannot_vouch_for_a_pinned_repo() {
        // The image tracks stable; the repo pins 1.98.1. Stable moves on —
        // the image cannot prove it runs 1.98.1, so it refuses.
        let d = decide(&pinned("1.98.1"), &known("stable", &[]), &[], IMAGE);
        assert!(
            matches!(d, Decision::RefuseToLocal { .. }),
            "moving stable vs pinned 1.98.1 must refuse, got {d:?}"
        );
    }

    #[test]
    fn pinned_image_cannot_satisfy_a_newer_pin() {
        let d = decide(
            &pinned("1.99.0"),
            &known_pinned("stable", "1.98.1", &[]),
            &[],
            IMAGE,
        );
        assert!(
            matches!(d, Decision::RefuseToLocal { .. }),
            "1.99.0 request vs 1.98.1 image must refuse, got {d:?}"
        );
    }

    #[test]
    fn dated_nightly_image_is_not_a_moving_nightly() {
        let d = decide(
            &pinned("nightly"),
            &known("nightly-2026-09-01", &[]),
            &[],
            IMAGE,
        );
        assert!(
            matches!(d, Decision::RefuseToLocal { .. }),
            "tracking nightly vs a dated nightly image must refuse, got {d:?}"
        );
    }

    #[test]
    fn unadvertised_feature_refuses_despite_a_channel_match() {
        let d = decide(
            &pinned("stable"),
            &known("stable", &[]),
            &features(&["sccache"]),
            IMAGE,
        );
        let Decision::RefuseToLocal { line, .. } = d else {
            panic!("a missing requested feature must refuse, got {d:?}")
        };
        assert!(
            line.contains("\"sccache\""),
            "the feature is named: {line:?}"
        );
        assert!(line.contains(IMAGE), "the image is named: {line:?}");
        assert!(line.contains("falling back to local execution"));
    }

    #[test]
    fn refused_detail_has_no_prefix_but_the_line_does() {
        let d = decide(&pinned("1.98.1"), &known("stable", &[]), &[], IMAGE);
        let Decision::RefuseToLocal { line, detail } = d else {
            panic!("expected a refusal")
        };
        assert!(detail.starts_with("refusing remote run"));
        assert_eq!(line, format!("{PARITY_PREFIX} {detail}"));
    }

    // --- unknown: warn-only ---------------------------------------------

    #[test]
    fn unlabeled_image_warns_but_submits() {
        let d = decide(
            &pinned("stable"),
            &unknown("the image publishes no org.gantry.* labels — nothing to compare against"),
            &[],
            IMAGE,
        );
        assert!(d.is_submit(), "warn-only: the submission proceeds");
        let Decision::SubmitWithWarning { line } = d else {
            panic!("an unlabeled image is warn-only, got {d:?}")
        };
        assert!(line.starts_with("[gantry]"), "warn line: {line:?}");
        assert!(line.starts_with(PARITY_PREFIX), "warn line: {line:?}");
        assert!(line.contains(IMAGE), "the image is named: {line:?}");
        assert!(
            line.contains("no org.gantry.* labels"),
            "the unknown reason rides the warning: {line:?}"
        );
    }

    #[test]
    fn lookup_failure_reason_rides_the_warning() {
        let d = decide(
            &pinned("stable"),
            &unknown("no read-only registry tool on PATH (tried skopeo, crane, docker)"),
            &[],
            IMAGE,
        );
        let Decision::SubmitWithWarning { line } = d else {
            panic!("a failed lookup is warn-only, got {d:?}")
        };
        assert!(line.contains("skopeo"), "reason text: {line:?}");
        assert!(line.starts_with(PARITY_PREFIX));
    }

    #[test]
    fn unpinned_repo_on_an_unknown_image_still_warns() {
        // Unknown wins over unpinned: the warning is about the image half.
        let d = decide(&LocalPin::Unpinned, &unknown("unlabeled"), &[], IMAGE);
        assert!(matches!(d, Decision::SubmitWithWarning { .. }));
        assert!(d.is_submit());
    }

    #[test]
    fn malformed_pin_on_a_known_image_warns() {
        let d = decide(
            &LocalPin::Malformed {
                reason: "rust-toolchain.toml is not valid TOML: broken".to_string(),
            },
            &known("stable", &[]),
            &[],
            IMAGE,
        );
        assert!(d.is_submit(), "warn-only: the submission proceeds");
        let Decision::SubmitWithWarning { line } = d else {
            panic!("an unusable pin is warn-only, got {d:?}")
        };
        assert!(line.starts_with(PARITY_PREFIX));
        assert!(
            line.contains("not valid TOML"),
            "the repo-side reason rides the warning: {line:?}"
        );
    }

    #[test]
    fn malformed_pin_with_a_missing_feature_still_refuses() {
        // The feature check is provable regardless of the pin's state — a
        // broken pin file cannot un-prove a missing feature.
        let d = decide(
            &LocalPin::Malformed {
                reason: "rust-toolchain.toml names no channel".to_string(),
            },
            &known("stable", &[]),
            &features(&["sccache"]),
            IMAGE,
        );
        assert!(
            matches!(d, Decision::RefuseToLocal { .. }),
            "missing feature refuses even with a malformed pin, got {d:?}"
        );
    }

    // --- invariants -------------------------------------------------------

    /// Every non-silent decision's line starts with the full parity prefix —
    /// the `[gantry]` head the loudness contract requires, `parity:` naming
    /// the subsystem.
    #[test]
    fn every_line_carries_the_gantry_parity_prefix() {
        let decisions = vec![
            decide(&pinned("stable"), &known("nightly", &[]), &[], IMAGE),
            decide(
                &pinned("stable"),
                &known("stable", &[]),
                &features(&["sccache"]),
                IMAGE,
            ),
            decide(&pinned("stable"), &unknown("unlabeled"), &[], IMAGE),
            decide(
                &LocalPin::Malformed {
                    reason: "no channel".to_string(),
                },
                &known("stable", &[]),
                &[],
                IMAGE,
            ),
        ];
        for d in &decisions {
            let line = d.loud_line().expect("every scenario here is non-silent");
            assert!(
                line.starts_with("[gantry] parity: "),
                "line must carry the loud prefix, got {line:?}"
            );
        }
    }

    /// The channel-satisfaction table: exactly the two provable shapes match
    /// — verbatim channel, or the image's pin being the request.
    #[test]
    fn channel_satisfaction_table() {
        let label = |channel: &str, pin: Option<&str>| CapabilityLabel {
            channel: channel.to_string(),
            pin: pin.map(str::to_string),
            features: Default::default(),
        };
        // Verbatim channel.
        assert!(channel_satisfied("stable", &label("stable", None)));
        assert!(channel_satisfied("nightly", &label("nightly", None)));
        // Pin refinement, both directions.
        assert!(channel_satisfied(
            "stable",
            &label("stable", Some("1.98.1"))
        ));
        assert!(channel_satisfied(
            "1.98.1",
            &label("stable", Some("1.98.1"))
        ));
        assert!(channel_satisfied(
            "nightly-2026-09-01",
            &label("nightly", Some("nightly-2026-09-01"))
        ));
        // Everything else refuses.
        assert!(!channel_satisfied("stable", &label("nightly", None)));
        assert!(!channel_satisfied("1.98.1", &label("stable", None)));
        assert!(!channel_satisfied(
            "1.99.0",
            &label("stable", Some("1.98.1"))
        ));
        assert!(!channel_satisfied(
            "nightly",
            &label("nightly-2026-09-01", None)
        ));
    }

    // --- LocalPin ---------------------------------------------------------

    #[test]
    fn local_pin_sorts_the_read_outcomes() {
        let request = ToolchainRequest {
            channel: "stable".to_string(),
            components: vec!["clippy".to_string()],
            targets: Vec::new(),
        };
        assert_eq!(
            LocalPin::from_read_result(Ok(Some(request.clone()))),
            LocalPin::Pinned(request)
        );
        assert_eq!(
            LocalPin::from_read_result(Ok(None)),
            LocalPin::Unpinned,
            "a missing file is an unpinned repo, not an error"
        );
        let LocalPin::Malformed { reason } =
            LocalPin::from_read_result(Err(ToolchainTomlError::MissingChannel))
        else {
            panic!("a parse error is Malformed")
        };
        assert_eq!(reason, "rust-toolchain.toml names no channel");
    }

    #[test]
    fn local_pin_read_maps_the_filesystem() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(
            LocalPin::read(&dir.path().join("absent.toml")),
            LocalPin::Unpinned
        );

        let path = dir.path().join("rust-toolchain.toml");
        std::fs::write(&path, "[toolchain]\nchannel = \"1.98.1\"\n").expect("write pin");
        assert_eq!(LocalPin::read(&path), pinned("1.98.1"));

        std::fs::write(&path, "[toolchain]\nchannel = 3\n").expect("write broken pin");
        assert!(matches!(LocalPin::read(&path), LocalPin::Malformed { .. }));
    }
}
