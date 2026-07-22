# gantry-rs

**Transparent offload shim for expensive `cargo` commands.** (Binary name: `gantry`.) A gantry crane moves cargo containers between ship and shore; `gantry` moves `cargo` workloads between your machine and a remote executor — without the caller (human or AI agent) having to know it exists.

You (or your coding agent) run plain `cargo test`. A PATH shim intercepts it, checks that the repo is clean and pushable, ships the exact commit to a remote executor (Argo Workflows, SSH host, or any command-template backend), streams the logs back, and returns the real exit code. If anything about the repo state or the backend makes remote execution unsafe or impossible, it degrades — loudly, never silently — to a resource-capped local run (cgroup CPU/memory limits), so a runaway test suite can never take down the shared box.

Extraction of a battle-tested in-house pair of bash scripts (`~/.local/bin/cargo` + `cargo-remote`) into a standalone, configurable, publishable tool: single static binary, curl-pipe installer, `doctor` command, transparent to agents.

## Status

**Planning.** No implementation yet — see [`docs/plan/plan.md`](docs/plan/plan.md).

## Structure

- `docs/notes/` — features, constraints, design decisions, naming rationale
- `docs/research/` — prior art survey and the in-house implementation this extracts from
- `docs/plan/plan.md` — complete application plan

## Why this exists

Fleets of AI coding agents (and humans) sharing one workstation all eventually run `cargo test`. Uncoordinated, that means load spikes, OOM-killed sessions, and dead agents. Existing tools either require the caller to invoke something different (`cargo remote test`, `earthly +test`, bazel) or only cache compilation (sccache). The one property none of them offer is **interception transparency**: the vanilla command keeps working, and policy decides where it actually runs. That is the product.
