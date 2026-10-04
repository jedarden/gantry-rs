# `org.gantry.toolchain` label payload contract — version 1

The OCI label a gantry builder image carries to declare the Rust toolchain it
builds with. **`src/toolchain.rs` is the single definition site**
(`TOOLCHAIN_LABEL`, `PAYLOAD_SCHEMA_VERSION`, `label_payload`,
`parse_label_payload`, `parse_local_toolchain`, `read_local_toolchain`,
`compare`); the intended producer is the builder-image build
(`ronaldraygun/gantry-builder` Dockerfiles, labeling themselves at build
time), the intended consumer is the submit-path parity preflight (plan
Component 5, argo bullet — not wired yet; this module is the pure half).
This note is the human-readable contract those sides must agree on, mirroring
`docs/notes/verdict-json-contract.md`'s role for verdict.json. It defines the
format that was unspecified before gantry-070c2ce8.

Plan anchors: §Component 5 ("Parity preflight: builder images carry
capability labels (`org.gantry.toolchain=…`); before submission gantry
compares local rust-toolchain.toml and requested features against them — a
mismatch is a loud local fallback (warn-only for unlabeled images), because a
verdict from the wrong toolchain is a wrong answer delivered confidently"),
§"Versioning & compatibility".

## The v1 payload

A JSON object — one string-valued label, so it survives every registry's
string→string label map. Canonical form (what `label_payload` emits, no
extraneous whitespace):

```json
{"schema_version":1,"channel":"stable"}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `schema_version` | integer | yes | Payload schema version. This build parses and emits exactly `1`; any other value is **not** read as v1 — the payload is uninterpretable (see below). |
| `channel` | string | yes | The rustup toolchain identity the image builds with: `"stable"`, `"1.83.0"`, a full `stable-x86_64-unknown-linux-gnu` style pin — whatever string rustup would resolve. |

Unknown fields inside a v1 payload are ignored (serde's default): `components`
is the anticipated future additive field (the plan's "requested features" half
of the preflight), and a producer may add it, or anything else, without
breaking this parser. Unknown **versions** are a different boundary: a payload
claiming `schema_version: 2` asserts a contract this parser does not know, so
it is never interpreted field-wise as v1 — the DD-10 strictness that
verdict.json pins for the same reason.

## Comparison semantics (`compare`)

`compare(label, local)` takes the image's label value (`None` = the image
carries no such label) and the repo's local pin (from
`read_local_toolchain`, which reads `rust-toolchain.toml`'s
`[toolchain].channel`). Exactly three verdicts:

| Label | Local pin | Verdict |
|---|---|---|
| absent / malformed / unknown version | anything | `Unlabeled` — parity cannot be asserted; the plan's warn-only case, never a refusal |
| readable, any channel | none (no `rust-toolchain.toml`, or no `[toolchain].channel`) | `Compatible` — a repo that pins nothing cannot mismatch |
| readable, `channel ==` pin | `channel` | `Compatible` |
| readable, `channel ≠` pin | `channel` | `Mismatch { expected: pin, found: label }` — the loud local fallback |

`expected` is the local pin (what the repo asks the image to provide);
`found` is what the label declares the image actually provides.

Channel equality is **exact string equality**, not rustup-equivalence:
`"1.83"` vs `"1.83.0"` is a `Mismatch`. The asymmetry justifies the
conservatism — a false `Mismatch` costs one local fallback run; a false
`Compatible` costs a verdict produced by the wrong toolchain, delivered
confidently. Looser equivalence (shorthand resolution, channel-family
matching) would need rustup's resolution rules in-process and is deliberately
not attempted in v1.

## Backward compatibility

- Absent label: images predating this contract simply have no label; the
  preflight reads them as `Unlabeled` and warns only — an unlabeled image is
  never refused (plan sentence above).
- Additive fields: free, ignored by this parser.
- New schema versions: this parser degrades them to `Unlabeled` rather than
  guessing; a build that speaks v2 ships a parser that says so.

## Provenance

Format defined and the pure comparison module shipped in gantry-070c2ce8
(auto-split grandchild 1/3 of gantry-a6745d06, builder-image parity
preflight, plan Component 5 argo v1). The submit-path wiring and the
mismatch-refusal integration test are the sibling children.
