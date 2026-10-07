# gantry-rs

**Transparent offload shim for expensive `cargo` commands.** (Binary name: `gantry`.) A gantry crane moves cargo containers between ship and shore; `gantry` moves `cargo` workloads between your machine and a remote executor — without the caller (human or AI agent) having to know it exists.

You (or your coding agent) run plain `cargo test`. A PATH shim intercepts it, checks that the repo is clean and pushable, ships the exact commit to a remote executor (Argo Workflows, SSH host, or any command-template backend), streams the logs back, and returns the real exit code. If anything about the repo state or the backend makes remote execution unsafe or impossible, it degrades — loudly, never silently — to a resource-capped local run (cgroup CPU/memory limits), so a runaway test suite can never take down the shared box.

Extraction of a battle-tested in-house pair of bash scripts (`~/.local/bin/cargo` + `cargo-remote`) into a standalone, configurable, publishable tool: single static binary, curl-pipe installer, `doctor` command, transparent to agents.

## Status

**Phase 0.5 (walking skeleton) and most of Phase 1a (core loop, hardened) are implemented** — shim dispatch, hardcoded/layered config with last-known-good fallback, GitGate, RunLog (write-ahead run records + orphan detection), doctor core, RefPusher (epoch-ref push), and both the local-command and Argo backends with a full verdict ladder. See [`docs/plan/plan.md`](docs/plan/plan.md) for the phase breakdown and what's still open (Phase 1b onward).

## Usage

The shim intercepts `cargo` on PATH and offloads eligible invocations automatically. For anything else — or whenever you want the offload decision in your own hands — `gantry run` invokes the same pipeline explicitly, no shimming required:

```
gantry run [--backend none|argo|command] -- <cmd> [args…]
```

```
gantry run -- make -j4
gantry run --backend argo -- cargo test --workspace
```

Everything after `--` is the wrapped command, verbatim — its own flags are never read by gantry, which is why the separator is required. The wrapped command routes through the same GitGate → push → backend pipeline (and the same capped-local fallback when the remote path is unavailable) as an intercepted `cargo test`, and lands in the run ledger the same way.

The exit code is the wrapped command's, not gantry's: a local run passes it through byte-exact, a remote run lands on the verdict ladder (0 pass, 1 failure, 2 infra). Usage errors — a missing `--`, an unknown `--backend` value — exit 2 before any gate, backend, or ledger is touched. `--backend` overrides the configured backend for this run only; `gantry explain` prints the active backend in the same spelling, so its output can be pasted straight back as `gantry run --backend <that>`.

## Structure

- `docs/notes/` — features, constraints, design decisions, naming rationale
- `docs/research/` — prior art survey and the in-house implementation this extracts from
- `docs/plan/plan.md` — complete application plan
- `src/` — implementation (see plan for module-to-phase mapping)
- `tests/integration.rs` — end-to-end round-trip tests against the fixtures in `tests/integration/fixtures/`

## Development

Work directly on `main`. This repo does not use per-bead `wip/*` feature branches —
an earlier phase of the project accumulated ~48 beads' worth of completed work
scattered across 17 never-merged `wip/<worker>/<bead-id>` branches, which is more
git-archaeology than a project this size should need. Commit straight to `main`;
merge commits are fine when real parallel work legitimately diverges, but don't
open a new long-lived branch as a matter of routine per-bead workflow.

## Why this exists

Fleets of AI coding agents (and humans) sharing one workstation all eventually run `cargo test`. Uncoordinated, that means load spikes, OOM-killed sessions, and dead agents. Existing tools either require the caller to invoke something different (`cargo remote test`, `earthly +test`, bazel) or only cache compilation (sccache). The one property none of them offer is **interception transparency**: the vanilla command keeps working, and policy decides where it actually runs. That is the product.
