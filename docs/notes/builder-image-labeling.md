# Labeling a builder image — `org.gantry.toolchain`

*Operator-facing how-to for the capability label a gantry builder image
carries to declare the Rust toolchain it builds with (plan Component 5, argo
bullet: "builder images carry capability labels (`org.gantry.toolchain=…`)");
the comment block in `contrib/argo/gantry-verify-workflowtemplate.yml` is the
in-repo summary of this page, and the two are kept in lockstep. The wire
format itself is specified in `docs/notes/toolchain-label-payload.md`; the
pairing of a module with its contract note mirrors `src/verdict.rs` /
`docs/notes/verdict-json-contract.md`. `src/toolchain.rs` is the single
definition site (`TOOLCHAIN_LABEL`, `PAYLOAD_SCHEMA_VERSION`). This page and
the workflowtemplate comments shipped in gantry-cd7d320c (auto-split child
5/5 of gantry-9d3c520c); the payload module in gantry-070c2ce8; doctor's
builder-image parity check in gantry-35136ce5.*

## The label

Key `org.gantry.toolchain`, one string-valued label — OCI label maps are
string→string, so the payload is JSON packed into that single string.
Canonical form (what `label_payload` emits, no extraneous whitespace):

```
{"schema_version":1,"channel":"<rustup channel>"}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `schema_version` | integer | yes | Payload schema version. This build parses and emits exactly `1`. |
| `channel` | string | yes | The rustup toolchain identity the image builds with: `"stable"`, `"1.83.0"`, or a fuller `stable-x86_64-unknown-linux-gnu` style pin. |

- Unknown fields inside a v1 payload are ignored (additive evolution is
  free — `components` is the anticipated future field for the plan's
  "requested features" half).
- A payload claiming any other `schema_version` is **not** read as v1: it is
  uninterpretable, which lands in the warn-only row below (never a refusal,
  never a guess).
- Label the image **at build time** (a `LABEL` in the Dockerfile). Do not
  push new content under an existing tag while its label grows stale — the
  label must describe the image actually at that tag.

## Labeling a builder image

### Dockerfile

```dockerfile
FROM rust:1.83.0-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends git jq \
 && rm -rf /var/lib/apt/lists/*
LABEL org.gantry.toolchain='{"schema_version":1,"channel":"stable"}'
```

The single quotes are shell-side; `LABEL` receives the JSON verbatim. `git`
and `jq` are in the picture because the template's container step needs them
(the stock `rust` image has neither — see the `builder-image` parameter
comment in the workflowtemplate).

### Build and push

```bash
docker build -t ronaldraygun/gantry-builder:1.83.0 -f Dockerfile.gantry-builder .
docker push ronaldraygun/gantry-builder:1.83.0
```

### Relabel an existing image without a rebuild

A FROM-only layer inherits everything else byte-for-byte and only adds the
label:

```bash
cat <<'EOF' | docker build -t ronaldraygun/gantry-builder:1.83.0 -
FROM ronaldraygun/gantry-builder:1.83.0
LABEL org.gantry.toolchain='{"schema_version":1,"channel":"stable"}'
EOF
docker push ronaldraygun/gantry-builder:1.83.0
```

### Verify — the exact read gantry uses

```bash
docker image inspect --format '{{json .Config.Labels}}' ronaldraygun/gantry-builder:1.83.0
```

That is the command doctor itself runs (consulting docker, then podman —
`IMAGE_RUNTIMES` in `src/doctor.rs`) and reads the key out of. If it prints
your label, parity checking is armed.

### Choosing `channel`

Comparison is **exact string equality** (`compare` in `src/toolchain.rs`),
against the repo's `rust-toolchain.toml` `[toolchain].channel` pin — not
rustup-equivalence: `"1.83"` vs `"1.83.0"` is a Mismatch. So:

- An image that resolves its toolchain through rustup's `stable` channel
  labels `"stable"` (this repo pins `"stable"`).
- An image hard-pinned to a version labels that version (`"1.83.0"`), and
  repos pinning `"stable"` will then read as Mismatch **by design** — align
  the two sides rather than loosening one.

The asymmetry is deliberate: a false Mismatch costs one local fallback run;
a false Compatible costs a verdict produced by the wrong toolchain,
delivered confidently. Looser matching is deliberately not attempted in v1.

## What gantry does with the label

| Label state | Local pin | doctor (`gantry doctor`, shipped) | Submit-path preflight (plan Component 5; wiring tracked as gantry-8a48c76a) |
|---|---|---|---|
| readable, `channel` == pin | present | pass | proceed |
| readable, `channel` ≠ pin | present | **FAIL** — "a verdict from the wrong toolchain is a wrong answer delivered confidently" | loud local fallback; the remote run never happens |
| absent, malformed, or unknown `schema_version` | any | **warning** — "parity cannot be asserted, submissions proceed warn-only" | warn-only; submission proceeds. **An unlabeled image is never refused** |
| readable (any channel) | none (no `rust-toolchain.toml`, or no `[toolchain].channel`) | pass (`Compatible` — a repo that pins nothing cannot mismatch) | proceed |
| image unknown to the local runtimes | any | warning — "pull it locally to enable the parity check" | n/a |

Doctor skip rules: Tier-0 mode, the command backend, or argo with no
`builder_image` configured all report the check as **skipped** rather than
inventing a target (with no `builder_image` set the template default applies
and its name is not known locally).

The full comparison matrix and its rationale live in
`docs/notes/toolchain-label-payload.md`; this page is the operator slice.

## Provenance

- Payload format + pure comparison module: gantry-070c2ce8 (commit
  `d7cf9cf4`), auto-split grandchild 1/3 of gantry-a6745d06.
- Doctor's builder-image parity check: gantry-35136ce5 (commit `f16b9abf`).
- Submit-path preflight wiring: gantry-8a48c76a (not on the lineage this
  page landed on — the doctor check is the operator-facing surface here).
- This page + the workflowtemplate comment block: gantry-cd7d320c (child
  5/5 of gantry-9d3c520c, plan Component 5).
