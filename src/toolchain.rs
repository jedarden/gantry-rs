// gantry — rust-toolchain.toml reader for parity preflight (plan Component 5).
//
// The preflight's local half: "before submission gantry compares local
// rust-toolchain.toml and requested features against [the image's capability
// labels]" (plan §Component 5, argo backend). This module reads the local
// file and extracts what the comparison needs — the pinned channel plus the
// requested components and targets — and nothing more. It does not resolve a
// toolchain, talk to rustup, or decide fallbacks; those are later children of
// the same split.
//
// Scope is deliberately minimal: the TOML form (`rust-toolchain.toml`,
// `[toolchain]` table) only. The legacy bare-channel `rust-toolchain` file is
// not read here — the repo pin itself is the TOML form (channel `stable` with
// rustfmt + clippy), and the preflight's mismatch question only arises for
// repos that pin at all.
//
// A missing file is a normal outcome, not an error: no rust-toolchain.toml
// means "no pin to compare" — the same warn-only side unlabeled images take
// (plan: parity preflight is "warn-only for unlabeled images"). A file that
// exists but names no channel, though, is malformed — rustup refuses such a
// file too — so that is a typed error, not a silent no-pin.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::Deserialize;

/// The toolchain a local run would use, as pinned by a rust-toolchain.toml.
///
/// `channel` is the pinned rustup channel (e.g. `stable`, `1.98.1`,
/// `nightly-2026-09-01`); `components` and `targets` are the requested
/// extras, in file order. These are *requests*, not facts about the installed
/// toolchain — the preflight compares them against what a builder image
/// claims ([`crate::labels::CapabilityLabel`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainRequest {
    /// The pinned channel string, verbatim from the file.
    pub channel: String,
    /// Requested components (`rustfmt`, `clippy`, …), file order.
    pub components: Vec<String>,
    /// Requested targets (`wasm32-unknown-unknown`, …), file order.
    pub targets: Vec<String>,
}

/// The rust-toolchain.toml shape this module parses. Unknown keys (`profile`,
/// `path`, future additions) are ignored — the reader extracts the three
/// fields the preflight compares and nothing else.
#[derive(Debug, Deserialize)]
struct ToolchainFile {
    toolchain: Option<ToolchainTable>,
}

#[derive(Debug, Deserialize)]
struct ToolchainTable {
    channel: Option<String>,
    components: Option<Vec<String>>,
    targets: Option<Vec<String>>,
}

/// Parse rust-toolchain.toml text into a [`ToolchainRequest`].
///
/// Text that is not valid TOML of the expected shape (syntax error, wrong
/// field type) is [`ToolchainTomlError::InvalidToml`]; text with no
/// `[toolchain]` table or no channel in it is
/// [`ToolchainTomlError::MissingChannel`] — a pin file that names no pin is
/// malformed, matching rustup's own refusal.
pub fn parse(text: &str) -> Result<ToolchainRequest, ToolchainTomlError> {
    let file: ToolchainFile =
        toml::from_str(text).map_err(|e| ToolchainTomlError::InvalidToml(e.to_string()))?;
    let table = match file.toolchain {
        Some(table) => table,
        None => return Err(ToolchainTomlError::MissingChannel),
    };
    let channel = table.channel.unwrap_or_default();
    if channel.trim().is_empty() {
        return Err(ToolchainTomlError::MissingChannel);
    }
    Ok(ToolchainRequest {
        channel,
        components: table.components.unwrap_or_default(),
        targets: table.targets.unwrap_or_default(),
    })
}

/// Read and parse the rust-toolchain.toml at `path`.
///
/// `Ok(None)` when the file does not exist — no pin is the normal,
/// unpinned-repo case, not a failure. Any other read failure (a directory in
/// the path, permissions) is [`ToolchainTomlError::Io`]; a file that exists
/// but fails to parse is the [`parse`] error, verbatim.
pub fn read(path: &Path) -> Result<Option<ToolchainRequest>, ToolchainTomlError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ToolchainTomlError::Io(e.to_string())),
    };
    parse(&text).map(Some)
}

/// Why a rust-toolchain.toml could not be turned into a
/// [`ToolchainRequest`]. Typed so the preflight's fallback reason names the
/// precise problem; tests match variants, never substrings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolchainTomlError {
    /// The file exists but is not valid TOML of the expected shape.
    InvalidToml(String),
    /// The file parsed but names no channel (no `[toolchain]` table, or no
    /// `channel` in it).
    MissingChannel,
    /// The file exists but could not be read (permissions, path is a
    /// directory, …).
    Io(String),
}

impl fmt::Display for ToolchainTomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolchainTomlError::InvalidToml(msg) => {
                write!(f, "rust-toolchain.toml is not valid TOML: {msg}")
            }
            ToolchainTomlError::MissingChannel => {
                write!(f, "rust-toolchain.toml names no channel")
            }
            ToolchainTomlError::Io(msg) => {
                write!(f, "rust-toolchain.toml could not be read: {msg}")
            }
        }
    }
}

impl std::error::Error for ToolchainTomlError {}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    // The repo's own pin (channel-only + components): the exact shape this
    // reader must handle in production, kept inline so the test does not
    // depend on the working tree.
    const REPO_PIN: &str =
        "[toolchain]\nchannel = \"stable\"\ncomponents = [\"rustfmt\", \"clippy\"]\n";

    // --- parse ----------------------------------------------------------

    #[test]
    fn parse_channel_only() {
        let req = parse("[toolchain]\nchannel = \"1.98.1\"\n").expect("channel-only parses");
        assert_eq!(req.channel, "1.98.1");
        assert!(req.components.is_empty());
        assert!(req.targets.is_empty());
    }

    #[test]
    fn parse_channel_with_components_and_targets() {
        let req = parse(
            "[toolchain]\n\
             channel = \"nightly-2026-09-01\"\n\
             components = [\"rustfmt\", \"clippy\"]\n\
             targets = [\"wasm32-unknown-unknown\"]\n",
        )
        .expect("full pin parses");
        assert_eq!(req.channel, "nightly-2026-09-01");
        assert_eq!(req.components, vec!["rustfmt", "clippy"]);
        assert_eq!(req.targets, vec!["wasm32-unknown-unknown"]);
    }

    #[test]
    fn parse_repo_pin_shape() {
        let req = parse(REPO_PIN).expect("the repo's own pin parses");
        assert_eq!(req.channel, "stable");
        assert_eq!(req.components, vec!["rustfmt", "clippy"]);
        assert!(req.targets.is_empty());
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let req = parse(
            "[toolchain]\n\
             channel = \"stable\"\n\
             profile = \"minimal\"\n\
             path = \"/opt/toolchain\"\n",
        )
        .expect("unknown keys are ignored");
        assert_eq!(req.channel, "stable");
    }

    #[test]
    fn parse_missing_toolchain_table_is_missing_channel() {
        let req = parse("other-table = 1\n").expect_err("no [toolchain] is malformed");
        assert_eq!(req, ToolchainTomlError::MissingChannel);
    }

    #[test]
    fn parse_channel_missing_or_empty_is_missing_channel() {
        assert_eq!(
            parse("[toolchain]\nprofile = \"minimal\"\n"),
            Err(ToolchainTomlError::MissingChannel)
        );
        assert_eq!(
            parse("[toolchain]\nchannel = \"\"\n"),
            Err(ToolchainTomlError::MissingChannel)
        );
    }

    #[test]
    fn parse_invalid_toml_is_invalid_toml() {
        let err = parse("[toolchain\nchannel = \"stable\"").expect_err("broken TOML fails");
        assert!(matches!(err, ToolchainTomlError::InvalidToml(_)));
    }

    #[test]
    fn parse_wrong_typed_field_is_invalid_toml() {
        let err = parse("[toolchain]\nchannel = \"stable\"\ncomponents = \"rustfmt\"\n")
            .expect_err("a bare string is not a component list");
        assert!(matches!(err, ToolchainTomlError::InvalidToml(_)));
    }

    // --- read -----------------------------------------------------------

    #[test]
    fn read_missing_file_is_no_pin_not_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("rust-toolchain.toml");
        assert_eq!(read(&missing).expect("missing file is Ok(None)"), None);
    }

    #[test]
    fn read_existing_file_returns_the_request() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rust-toolchain.toml");
        std::fs::write(&path, REPO_PIN).expect("write pin");
        let req = read(&path).expect("read succeeds").expect("pin present");
        assert_eq!(req.channel, "stable");
        assert_eq!(req.components, vec!["rustfmt", "clippy"]);
    }

    #[test]
    fn read_directory_path_is_io_error() {
        let dir = TempDir::new().expect("tempdir");
        let err = read(dir.path()).expect_err("reading a directory fails");
        assert!(matches!(err, ToolchainTomlError::Io(_)));
    }

    #[test]
    fn read_existing_but_malformed_file_is_parse_error() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("rust-toolchain.toml");
        std::fs::write(&path, "[toolchain]\nprofile = \"minimal\"\n").expect("write pinless");
        assert_eq!(
            read(&path).expect_err("pinless file fails"),
            ToolchainTomlError::MissingChannel
        );
    }

    #[test]
    fn errors_are_std_errors_with_display() {
        let err: Box<dyn std::error::Error> = Box::new(ToolchainTomlError::MissingChannel);
        assert!(!err.to_string().is_empty());
    }
}
