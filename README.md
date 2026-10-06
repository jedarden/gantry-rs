# gantry-rs

**Transparent offload shim for expensive `cargo` commands.** (Binary name: `gantry`.) A gantry crane moves cargo containers between ship and shore; `gantry` moves `cargo` workloads between your machine and a remote executor — without the caller (human or AI agent) having to know it exists.

You (or your coding agent) run plain `cargo test`. A PATH shim intercepts it, checks that the repo is clean and pushable, ships the exact commit to a remote executor — an SSH host, an Argo Workflows cluster, or any command-template backend — streams the logs back, and returns the real exit code. If anything about the repo state or the backend makes remote execution unsafe or impossible, it degrades — loudly, never silently — to a resource-capped local run (cgroup CPU/memory limits), so a runaway test suite can never take down the shared box.

## The onboarding ladder

Gantry is useful at every rung; a rung is an upgrade, never a prerequisite.

### Tier-0 — zero config: install it and it already works

With no configuration file present at all, gantry is a pure local cap-wrapper: intercepted `cargo test` still runs locally, but under a per-run cgroup cap (default 200% CPU / 6G memory) with a write-ahead run record, and nothing goes remote. A `[gantry]` line notes the active tier at most once per hour, so transcripts see it without per-run noise. Install → value with zero setup; remote offload is an upgrade.

The zero-config proof is `gantry quickcheck` — three checks (shim hand-off resolves the real binary, a cap is actually creatable, git plumbing works), no backend, no network, exit 0 iff all pass (output illustrative):

```console
$ gantry quickcheck
[gantry] quickcheck: shim resolves: ok — real binary: …
[gantry] quickcheck: cap works: ok — …
[gantry] quickcheck: git ok: ok — …
[gantry] quickcheck: passed (1.2s)
```

### Tier-1 — one SSH host (~10 minutes, no Kubernetes)

The flagship quickstart. Point gantry at any host ssh can reach (`user@host`, or an alias from `~/.ssh/config`) and it performs the whole setup — no Kubernetes, no cluster, one box with git and rustup:

```console
$ gantry init --ssh user@host
```

The flow is a one-way ladder of legs; every leg that can fail names itself in the message so you land on the fix, and no config is written until the host has proven it can carry a run:

1. **reach** — the host answers over ssh, and the executor paths become absolute.
2. **git** — `git --version` runs on the host: the executor fetches the epoch ref with git, so a git that is present but cannot run fails here, not mid-run.
3. **cargo** — cargo is located (PATH, then the rustup install at `$HOME/.cargo/bin`) and *proven to run*; the probed absolute path is pinned into the installed wrapper, so the executor's `cargo test` inherits the same answer the probe verified.
4. **install** — the reference executor (`contrib/gantry-exec.sh` — the very copy `doctor --e2e` runs) plus a thin wrapper are written to `<home>/.local/bin` on the host.
5. **preset** — your user config (`~/.config/gantry/config.toml`) is written with `backend = "command"` and submit/logs/wait templates that drive the installed wrapper over ssh. A pre-existing config is backed up to `<config>.init-backup`, never destroyed.
6. **e2e** — the `doctor --e2e` canary finishes the run and owns the exit code: `E2E test: …` and exit 0 when the canary is green, `E2E test failed: …` on stderr and exit 1 when it is not.

Exit codes: **0** onboarded with the canary green, **1** a leg failed (the message names the leg and the fix), **2** usage error.

### Tier-2 — Argo Workflows (advanced)

For fleets that already run [Argo Workflows](https://argoproj.github.io/), gantry submits each run as a Workflow against a template you control and streams the logs back through the verdict ladder (0 pass, 1 failure, 2 infra). Install the reference template from [`contrib/argo/gantry-verify-workflowtemplate.yml`](contrib/argo/gantry-verify-workflowtemplate.yml), then opt in via the user config:

```toml
[remote]
backend   = "argo"
ci_remote = "origin"

[remote.argo]
kubeconfig    = "~/.kube/config"
namespace     = "argo-workflows"
template      = "gantry-verify"
generate_name = "gantry-"
```

Each run pushes the already-committed HEAD to the configured git remote as an epoch ref (`refs/gantry/<epoch>-<sha>`) and dispatches that exact revision — no branch pushes, ever. When the cluster is unreachable, gantry degrades loudly to the capped-local fallback, and `gantry explain` tells you which backend and ref a run *would* use before you spend anything.

## Install

Releases are cut by CI (`gantry-ci` on Argo Workflows): fmt + clippy + tests must pass on the released commit, then static musl tarballs and a `checksums.txt` are attached to the tagged release. The installer fetches the latest release, lays the binary and the `cargo` shim into `~/.local/bin`, and finishes by running `gantry quickcheck`:

```console
$ curl -fsSL https://git.ardenone.com/jedarden/gantry-rs/raw/branch/main/install.sh | sh
```

`GANTRY_INSTALL_FORGE`, `GANTRY_INSTALL_REPO`, and `GANTRY_INSTALL_VERSION` retarget the installer.

> **Note:** the Forgejo instance hosting releases currently answers anonymous raw and release fetches with a sign-in redirect, so the curl-pipe above returns HTML until that is lifted. Until then, install from the anonymously cloneable GitHub mirror:
>
> ```console
> $ git clone https://github.com/jedarden/gantry-rs && cd gantry-rs
> $ cargo build --release
> $ install -m755 target/release/gantry ~/.local/bin/gantry
> $ ln -sf gantry ~/.local/bin/cargo    # the shim: a PATH dir ahead of the real toolchain
> $ gantry quickcheck
> ```

The shim is just a `cargo` symlink (or copy) onto the gantry binary in a PATH directory ahead of the real toolchain; `GANTRY_LOCAL=1 cargo test` forces local passthrough at any time. `gantry uninstall` reverses the whole layout — binary, shims, state, config (`--dry-run` to preview).

## Configuration in one minute

Three layers, last-known-good fallback: system → user (`~/.config/gantry/config.toml`) → repo (`.gantry.toml`). The repo layer may *narrow* behavior (add intercepts, pin `backend = "none"`) but may **not** change `ci_remote`, `push_mode`, or credential-adjacent keys — repo config is data from the repo, and a cloned repo must not be able to redirect pushes. With `backend = "none"` (the zero-config default) gantry stays Tier-0 cap-only.

## Diagnostics

| Command | What it does |
|---|---|
| `gantry quickcheck` | ~30s no-backend sanity: shim, cap, git — the Tier-0 proof |
| `gantry doctor` | Health checks |
| `gantry doctor --e2e` | End-to-end canary through the real pipeline |
| `gantry doctor --drill` | Fault-injection fire drill |
| `gantry why` | Replay the last run's gate/decision trace (`--json` for machines) |
| `gantry explain` | Dry run: gates, backend, exact ref — no network |
| `gantry status` | Recent and in-flight runs (`--json`, `--limit N`) |
| `gantry report <id>` | A run's REDACTED crash bundle (`--package <dir>` to copy out) |
| `gantry run [--backend B] -- <cmd…>` | Offload an arbitrary command without shimming; exit code is the wrapped command's |
| `gantry init --ssh <target>` | SSH onboarding (Tier-1 above) |
| `gantry uninstall` | Remove shims, binary, state, and config |

Kill switches: `gantry off` disables interception entirely, `gantry on` re-enables it, `GANTRY_ON=1` overrides the state file per invocation.

## Security: what gantry touches, and the no-phone-home pledge

[`docs/notes/threat-model.md`](docs/notes/threat-model.md) is the published threat model; every claim in it names the code or test that enforces it. The shape: gantry reads your git metadata read-only, writes only its own state dir (never inside the repo — the GitGate refuses to run unless the tree is clean, so only committed revisions are ever shipped), mutates the git remote in exactly two shapes (create and GC its own `refs/gantry/<epoch>-<sha>` epoch refs — no branch pushes, no fetch/pull/merge), and opens network connections to the configured backend and the git remote.

That last sentence is the **INV-6 pledge** — no connections except the configured backend and the git remote — and it is enforced, not promised: `tests/no_phone_home_integration.rs` runs the binary under an `LD_PRELOAD` network observer and fails if anything else connects. The test is part of the CI quality gates, which is what makes the pledge a property of the binary rather than prose in this README.

## Status

Phases 0.5 through 2 are implemented and hardened: shim dispatch, layered config, GitGate, RunLog, epoch-ref pushing, the local/Argo/command backends with the verdict ladder, capped-local fallback, the diagnostics table above, SSH onboarding (`init --ssh`), the uninstaller, and the no-phone-home test. Phase 3 (release packaging and publication) is in flight — see [`docs/plan/plan.md`](docs/plan/plan.md) for the phase breakdown and what's still open.

## Repository structure

- `docs/notes/` — features, constraints, design decisions, naming rationale; includes the published [`threat-model.md`](docs/notes/threat-model.md)
- `docs/research/` — prior art survey and the in-house implementation this extracts from
- `docs/plan/plan.md` — complete application plan
- `src/` — implementation (see plan for module-to-phase mapping)
- `tests/` — integration suites (Tier-0, init --ssh, uninstall, no-phone-home, …)
- `contrib/` — the reference SSH executor and the Argo WorkflowTemplate

## Development

Work directly on `main`. This repo does not use per-bead `wip/*` feature branches —
an earlier phase of the project accumulated ~48 beads' worth of completed work
scattered across 17 never-merged `wip/<worker>/<bead-id>` branches, which is more
git-archaeology than a project this size should need. Commit straight to `main`;
merge commits are fine when real parallel work legitimately diverges, but don't
open a new long-lived branch as a matter of routine per-bead workflow.

## Why this exists

Fleets of AI coding agents (and humans) sharing one workstation all eventually run `cargo test`. Uncoordinated, that means load spikes, OOM-killed sessions, and dead agents. Existing tools either require the caller to invoke something different (`cargo remote test`, `earthly +test`, bazel) or only cache compilation (sccache). The one property none of them offer is **interception transparency**: the vanilla command keeps working, and policy decides where it actually runs. That is the product.
