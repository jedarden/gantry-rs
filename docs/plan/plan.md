# gantry — Application Plan

## Overview

`gantry` is a transparent offload shim for expensive developer commands, cargo-first. A shim binary named `cargo` sits ahead of the real toolchain in PATH; when an intercepted subcommand (default: `test`) is run inside a clean, pushable git repo, gantry ships the exact commit to a pluggable remote executor, streams logs back, and returns a faithful exit code. In every other case it runs the command locally under a cgroup resource cap. Callers — humans, scripts, AI agent fleets — never change what they type.

Extraction and hardening of the in-house `~/.local/bin/cargo` + `cargo-remote` bash pair (see `docs/research/in-house-implementation.md`); packaging follows the single-static-binary + installer + `doctor` pattern surveyed in `docs/research/prior-art.md`.

## Goals

1. Drop-in transparency: `cargo test` keeps working everywhere, with zero caller-side changes.
2. Never lose a test result: every invocation ends in a real verdict (remote or capped-local), every degradation is printed and logged.
3. Never mutate branch state: code sync uses hidden content-addressed refs only.
4. Pluggable remote execution: Argo Workflows first-class, generic command-template backend for everyone else, SSH later.
5. Protect the shared machine: production-proven cgroup caps on all local execution paths.
6. Publishable quality: single static binary, installer, `doctor`, docs that stand alone without this infrastructure.

## Non-goals

Compilation caching (compose with sccache), hermetic/Bazel-style execution, dirty-tree syncing, per-agent hook integrations, Windows, nextest interception (v1). See `docs/notes/features.md` for rationale.

## Architecture

```
caller (human / agent / script)
   │  runs `cargo test …`
   ▼
[shim dir]/cargo ──symlink──► gantry binary
   │  argv[0]=cargo → tool profile "cargo"
   ├─ subcommand not intercepted ───────────────► LocalExecutor (capped) → real cargo
   ├─ GANTRY_LOCAL=1 / disabled / no config ────► LocalExecutor (capped) → real cargo
   ▼
DecisionEngine
   ├─ GitGate: work tree? remote? clean (porcelain)? HEAD?
   │     └─ any "no" → LocalExecutor (reason printed)
   ▼
RefPusher: git push <ci_remote> <sha>:refs/gantry/<sha>
   ▼
Backend (trait RemoteBackend)
   ├─ argo:    create Workflow(templateRef, params) → stream pod logs → poll status.phase
   ├─ command: run user-templated submit/logs/wait argv arrays
   └─ ssh preset: command-template preset + contrib/gantry-exec.sh (flagship onboarding)
   ▼
Verdict
   ├─ Pass ──────────────► exit 0
   ├─ TestFailure ───────► exit code (no local retry — DD-4)
   └─ InfraFailure ──────► LocalExecutor (capped) → real cargo
                                   │
                          RunLog (~/.local/state/gantry/runs.jsonl)
```

Cross-cutting: `RunLog` records every decision; `Locks/JoinTable` (v1.x) dedups concurrent identical runs; `doctor` validates the whole chain.

## Components

### 1. Shim & dispatcher (`main.rs`)
- Dispatch on `argv[0]`: `cargo` → cargo tool profile; `gantry` → management CLI (`doctor`, `on`/`off`, `run`, `config`, `version`, `install-shims`).
- Real-binary resolution: config override `real_binary`, else PATH lookup with gantry's own shim dir stripped (self-recursion guard: refuse to exec a path whose canonical target is the gantry binary).
- Passthrough path budget: no git/network/config-parse beyond a single mmap'd config read; target < 5 ms overhead.

### 2. DecisionEngine
- Intercept set from config (`[tool.cargo] intercept = ["test"]`).
- Kill switches: `GANTRY_LOCAL=1`, `gantry off` (state file).
- Config-failure policy (resolves Q-7 via last-known-good): on parse failure, serve the persisted last-good config snapshot from the state dir and print an escalating `[gantry] config broken since <ts>, N runs degraded` banner on every intercepted run until fixed; with no snapshot, fail open to plain passthrough with the same banner. Behavior is never derived from a half-parsed file — a broken gantry must never block builds, and must never rot silently either.

### 3. GitGate
- Checks in order, each with a printed reason on failure: inside work tree → `HEAD` resolves → configured remote exists → `git status --porcelain` empty.
- All git interaction shells out to system `git` (no libgit2 — matches predecessor behavior, honors user's git config/credentials/hooks).

### 4. RefPusher
- `git push <ci_remote> <sha>:refs/gantry/<sha>` (`ci_remote` defaults to `origin`).
- Push failure = InfraFailure → local fallback.
- Optional `push_mode = "branch"` legacy escape hatch (explicit opt-in; prints a warning naming the risk).
- GC (resolves Q-3 via leases): refs are named `refs/gantry/<epoch>-<sha>`; each run records a client-side lease (ref, run-id, expiry) in the state dir, and every successful push opportunistically sweeps remote refs whose epoch has expired and whose runs are terminal — no cron, no daemon, and a sweep can never delete a ref an in-flight run still holds a lease on. Phase 2 design note must settle the epoch-vs-content-addressing tension (the same sha pushed twice yields two refs).

### 5. RemoteBackend trait + implementations

```rust
trait RemoteBackend {
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError>;
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError>; // best-effort
    fn wait(&self, h: &RunHandle, deadline: Instant) -> Result<Verdict, BackendError>;     // authoritative
    fn describe(&self, h: &RunHandle) -> String;  // human URL/name for "see details at …"
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError>;                            // Ctrl-C propagation
}
```

- **`argo`** (v1): builds the Workflow manifest with serde (no string splicing), submits via `kubectl create -f -` (kubeconfig path from config; direct REST API is a later option), polls pods for streaming, polls `status.phase` for the verdict, falls back to reading `outputs.parameters.output` when podGC ate the logs. Ships `contrib/argo/gantry-verify-workflowtemplate.yml` — a genericized `rust-verify` (parameters: `repo`, `revision`, `args-json`, `builder-image`; sccache optional). Remote contract (resolves Q-2 as faithful-argv): the remote executes exactly the argv the caller typed; check/clippy run only as opt-in gates from trusted user config, reported as separately-labeled `[gantry] gate:` results, never conflated with the test verdict. The template also emits a structured `verdict.json` output parameter (phase, exit code, oom, deadline — derived from workflow node status): OOMKilled and deadline-exceeded classify as InfraFailure → fallback, never TestFailure. Backends without verdict.json (command templates) degrade gracefully to exit-code-only interpretation.
- **`command`** (v1): three user-configured argv arrays (`submit`, `logs`, `wait`) with `{repo}` `{rev}` `{args_json}` `{handle}` placeholder substitution at the argv level; `submit`'s stdout is the handle; `wait`'s exit code is the verdict (0 pass, 1 test-failure, ≥2 infra-failure — documented contract).
- **SSH-first onboarding** (v1, flagship): not a third backend implementation — a shipped command-template *preset* plus `contrib/gantry-exec.sh` (~150-line POSIX executor that clones `refs/gantry/*` and runs the command), which doubles as executable documentation of the remote contract. "Remote = any spare SSH box" is the documented quickstart path; Argo is the "advanced" backend. A native `ssh` backend remains possible later only if the preset proves limiting.

### 6. LocalExecutor
- `systemd-run --scope --user -p CPUQuota={n}% -p MemoryMax={m} -p MemorySwapMax=0 -q -- <real> <args…>`; on failure to create a scope (no user session / container), plain `exec`.
- Applies to: intercepted-command fallbacks always; all passthrough invocations when `cap_passthrough = true` (default, matching predecessor).
- Fallback admission semaphore: capped local *fallback* runs additionally acquire a flock-based token file (default 3 concurrent per box) and queue with loud `[gantry] waiting for local slot (N ahead)` lines and a bounded wait — a remote outage under ~20 agents degrades as a serialized trickle, not a stampede that recreates the meltdown gantry exists to prevent. Kernel lock release means a SIGKILLed agent can never leak a slot.

### 7. RunLog & UX
- stderr lines prefixed `[gantry]`: decision, workflow name/URL, fallback reason, verdict. Written for agent transcripts as much as humans.
- `~/.local/state/gantry/runs.jsonl`: write-ahead form — an OPEN intent record is appended *before* dispatch, and every exit path must append a matching terminal verdict record; `gantry doctor` flags orphaned OPEN records as lost runs, making a silently-skipped run structurally detectable rather than an absence of evidence. O_APPEND single-line writes for concurrency safety.
- Intent records capture all gate inputs (tree state, remote, kill-switch state, chosen backend) so `gantry why` can replay the decision truthfully after the fact.

### 8. doctor / installer
- `gantry doctor`: shim-before-real PATH ordering; real binary resolves and is not gantry; config parses; backend preflight (`kubectl` reachable / template exists, or command-backend binaries exist); systemd-run scope creation probe; git identity present; orphaned intent records reported as lost runs.
- `gantry doctor --e2e`: canary run — pushes a tiny known-good fixture ref through the real RefPusher and backend and asserts the expected pass verdict round-trips. One command answers "is the pipeline actually working" on the operator's schedule, not at 9am when 20 agents degrade.
- `gantry doctor --drill`: fault-injection fire drill — injects a synthetic InfraFailure through a drill-scoped hook (never honored outside a drill) and asserts the entire loud-degrade chain fires: banner, intent/verdict records, semaphore-gated capped local run, correct exit code. The fallback path is gantry's core safety promise and otherwise only executes when things are already on fire.
- `install.sh`: detect platform → fetch release binary → install to `~/.local/bin/gantry` → create shim symlink(s) in a configured shim dir → verify PATH ordering (instruct if wrong) → write starter config → run `doctor`. `gantry uninstall` reverses it.
- Onboarding must be smooth and automatic: `gantry init --ssh user@host` performs the whole SSH-first setup — verifies git+cargo on the target, writes the command-template preset config, and finishes with `doctor --e2e`. Install to first remote run in ~10 minutes, no Kubernetes required.

### 9. JoinTable (v1.x)
- Key: `(remote_url, sha, tool, args)` hash. A flock'd state dir maps in-flight keys → RunHandle; a second invocation attaches to the existing handle (stream + wait) instead of resubmitting.

## Data models

### Config — `~/.config/gantry/config.toml` (layered: /etc/gantry → user → repo `.gantry.toml`)

```toml
[local]
cpu_quota_pct = 200
memory_max    = "6G"
cap_passthrough = true

[tool.cargo]
intercept   = ["test"]
# real_binary = "/home/user/.cargo/bin/cargo"   # optional override

[remote]
backend    = "argo"          # argo | command | ssh | none
ci_remote  = "origin"
push_mode  = "ref"           # ref | branch (legacy, warns)
deadline_minutes = 40

[remote.argo]
kubeconfig = "~/.kube/iad-ci.kubeconfig"
namespace  = "argo-workflows"
template   = "gantry-verify"
generate_name = "gantry-"

[remote.command]
# submit = ["my-ci", "submit", "--repo", "{repo}", "--rev", "{rev}", "--args", "{args_json}"]
# logs   = ["my-ci", "logs", "{handle}", "--follow"]
# wait   = ["my-ci", "wait", "{handle}"]
```

Repo-level `.gantry.toml` may narrow behavior (add intercepts, set `backend = "none"`) but may **not** change `ci_remote`/`push_mode`/credential-adjacent keys — repo config is data from the repo, and a cloned repo must not be able to redirect pushes (security consideration S-2).

### RunSpec / RunRecord

```rust
struct RunSpec { tool: String, subcommand: String, args: Vec<String>,
                 repo_url: String, sha: String, cwd_rel: PathBuf }

// runs.jsonl — one per invocation
{ "ts": …, "tool": "cargo", "args": ["test","--","--nocapture"],
  "repo": "…", "sha": "…", "decision": "remote|local",
  "reason": "clean|dirty|no_remote|disabled|env|infra_fallback:…",
  "backend": "argo", "handle": "gantry-x7k2p", "verdict": "pass|test_failure|infra_failure",
  "exit_code": 0, "durations_ms": { "gate": 40, "push": 900, "queue": 12000, "run": 341000 } }
```

### Verdict semantics (the load-bearing enum)

| Verdict | Meaning | Action |
|---|---|---|
| `Pass` | remote ran suite, passed | exit 0 |
| `TestFailure` | remote ran suite, failed | exit non-zero; **no local retry** |
| `InfraFailure` | no verdict produced (push/submit/schedule/stream/deadline) | capped local run; its exit code wins |

OOMKilled pods and deadline-exceeded workflows are `InfraFailure` by definition (via the remote contract's `verdict.json`): an infrastructure cap firing says nothing about the code, and reporting it as a test failure would fake a result.

## CLI surface

```
cargo <anything>            # via shim — the entire point
gantry doctor [--e2e|--drill]        # verify install; canary the real pipeline; fire-drill the fallback chain
gantry init --ssh user@host          # automatic SSH-first onboarding (preset + contrib executor + e2e check)
gantry why                  # replay the last run's gate/decision trace ("clean tree: NO ← here")
gantry explain -- <cmd…>    # dry run: what WOULD happen (gates, backend, exact ref) — no network
gantry on | off             # global toggle (state file)
gantry run [--backend B] -- <cmd…>   # explicit offload, no shim needed (v1.x)
gantry status               # recent runs, in-flight runs
gantry install-shims | uninstall
gantry config [--show-origin]
```

Diagnostics are agent-first: `why`, `explain`, and `status` take `--json` with a stable schema, so a fleet agent that hits a degradation can explore and self-diagnose (why did this run locally? is the remote down for everyone?) without a human reading stderr.

## Failure modes (exhaustive, each with defined behavior)

| Failure | Class | Behavior |
|---|---|---|
| not a git repo / no remote / dirty / untracked files | gate | local capped run, reason printed |
| ref push rejected (auth, network) | infra | local capped run |
| submit error (kubectl/exec fails) | infra | local capped run |
| pod never scheduled by deadline | infra | **local capped run** (predecessor wrongly exited 1) |
| log stream lost mid-run | benign | keep waiting on status; fetch output param at end |
| remote deadline exceeded | infra | print handle URL, local capped run |
| remote suite fails | test | exit non-zero, stop |
| remote pod OOMKilled / deadline-exceeded | infra | `verdict.json` classifies as InfraFailure → capped local run — never reported as a test failure |
| gantry process killed mid-run | self | orphaned OPEN intent in runs.jsonl; `gantry doctor` reports it as a lost run |
| remote outage → mass fallback | env | admission semaphore caps concurrent local runs (default 3), loud queue-position lines |
| Ctrl-C during remote run | user | propagate `cancel()`, exit 130 |
| gantry config broken/missing | self | serve last-known-good snapshot + escalating banner; plain passthrough if no snapshot (never silent) |
| systemd-run unavailable | env | plain exec (documented: containers self-limit) |
| gantry binary deleted but shim remains | self | shim *is* the binary (symlink) — impossible state; doctor checks dangling links |

## Security considerations

- **S-1 no implicit branch pushes** (DD-2) — the predecessor's worst incident class.
- **S-2 repo config is untrusted**: `.gantry.toml` cannot alter push targets, backends' credentials, or command templates. Only user/system config can define executable command templates.
- **S-3 no credential storage**: backends use ambient credentials (kubeconfig file path, SSH agent, git credential helpers). Gantry never reads/writes tokens.
- **S-4 arg hygiene** (DD-7): args as arrays end-to-end; remote receives `args_json`; no shell interpolation anywhere.
- **S-5 log hygiene**: run records store the remote URL as given but never credentials embedded in URLs (strip userinfo before logging).

## Implementation phases

### Phase 0 — Repo, docs, contract ✅ (this commit)
Scaffold, research, features, decisions, plan.

### Phase 1 — MVP binary (parity + footgun fixes)
Rust workspace, single `gantry` crate. Shim dispatch, config (with last-known-good snapshot + escalating broken-config banner), GitGate (porcelain), RefPusher (epoch refs + leases), `argo` + `command` backends, LocalExecutor (with fallback admission semaphore), Verdict ladder, write-ahead RunLog (intent + verdict records, gate inputs captured), `gantry why`/`explain`, `GANTRY_LOCAL`/`on`/`off`, `doctor` (core checks).
**Acceptance:** on a clean repo, `cargo test` runs remotely with streamed logs and correct exit codes for pass/fail; dirty repo falls back with reason; pod-never-scheduled falls back locally (not exit 1); `cargo test -- "weird: [args]"` round-trips intact; passthrough overhead < 5 ms (hyperfine); no branch ref moves on the remote (assert via `git ls-remote`); SIGKILLing gantry mid-run leaves an orphaned intent that `doctor` reports; 20 simultaneous fallback invocations serialize through the semaphore (stampede test); a corrupted config file produces last-known-good behavior + banner, never silence.

### Phase 2 — Remote contract & template
`contrib/argo/gantry-verify-workflowtemplate.yml` (genericized rust-verify: `args-json` param, ref-fetch clone of `refs/gantry/*`, optional sccache, faithful-argv default with opt-in labeled gates, `verdict.json` output), `contrib/gantry-exec.sh` shell reference executor, output-param log recovery, deadline config, Ctrl-C cancel, refs GC sweep, `doctor --e2e`/`--drill`, epoch-vs-content-addressing design note.
**Acceptance:** template applied on iad-ci; end-to-end run against a real repo; killing the client mid-run cancels the workflow; podGC'd fast run still prints full log from output param; an OOM-fixture suite classifies as InfraFailure and falls back (never "tests failed"); clippy findings appear as `[gantry] gate:` lines without failing a plain `cargo test`; stale refs reach zero at steady state; `doctor --drill` proves the full degrade chain end-to-end.

### Phase 3 — Onboarding, packaging & migration
musl static build; CI via Argo (`gantry-ci` WorkflowTemplate → tagged release, following the forge-ci/needle-ci pattern); `install.sh` + `gantry uninstall`; **SSH-first onboarding as the flagship quickstart** — `gantry init --ssh user@host` performs the whole setup automatically (verify target, write preset config, finish with `doctor --e2e`) and the README leads with it, documenting Argo as the advanced backend; agent-first `--json` diagnostics (`why`/`explain`/`status`) ship here so both humans and fleet agents can self-serve when something degrades; README rewritten for external users.
**Migration:** replace `~/.local/bin/cargo` + `cargo-remote` on ex44 and lab with gantry shims; config reproduces current behavior (argo backend → `rust-verify` initially); NEEDLE fleet inherits via PATH with zero changes; keep bash scripts as `.bak` for one week; watch `runs.jsonl` across a fleet cycle.
**Acceptance:** one week of NEEDLE production traffic with no silently-skipped runs (audit runs.jsonl vs worker transcripts) and no branch pushes originating from gantry.

### Phase 4 — Fleet features (v1.x)
JoinTable dedup, `gantry run`, `gantry status`, crates.io publish (name pending — open question Q-1). Native `ssh` backend dropped from this phase — superseded by the Phase 3 SSH-first preset; revisit only if the preset proves limiting.
**Acceptance:** two concurrent identical `cargo test` invocations produce one workflow, both callers get the verdict.

### Phase 5 — Stretch
Multi-tool profiles (`pytest`, `go test`) behind the same config schema; nextest interception + `--partition` sharding across pods; backend REST mode for Argo (drop kubectl dependency).

## Open questions

- **Q-1 crates.io name.** Is bare `gantry` registered/squatted? Check at publish time; fallbacks `gantry-rs` (matches the repo) / `gantry-shim`. Repo name stays `gantry-rs` regardless; binary stays `gantry`.
- **Q-4 `builder-image` selection.** Per-repo override belongs in `.gantry.toml` (it's not credential-adjacent) — confirm this doesn't violate S-2 in review.
- **Q-5 macOS local cap.** No systemd; `ulimit`/`taskpolicy` are weak substitutes. Ship uncapped-with-warning, or refuse to `cap_passthrough` on macOS?
- **Q-6 output-param size limits.** Argo output parameters have size caps (~256KB default); huge test logs will truncate. Artifact-based log capture (S3) as the durable channel?
## Resolved questions (2026-07-21 ideation run — full record in docs/notes/ideas-ledger.md)

- **Q-2 → faithful-argv.** The remote executes exactly the argv the caller typed; check/clippy are opt-in gates from trusted user config, reported as separately-labeled `[gantry] gate:` results. The in-house config keeps the gates on.
- **Q-3 → lease-based GC.** Epoch-named refs (`refs/gantry/<epoch>-<sha>`) plus client-side leases; every push opportunistically sweeps expired, terminal refs. No cron, no daemon; sweeps can never race an in-flight clone.
- **Q-7 → last-known-good.** Broken config serves the persisted last-good snapshot with an escalating banner (plain passthrough + banner if no snapshot exists). Never silent, never blocking, never derived from a half-parsed file.

Also adopted in that run (finalists 1–9 of 10; finalist 10, the demo/exit-code-oracle fixture, was declined): verdict.json infra-classification, write-ahead intent ledger, fallback admission semaphore, SSH-first onboarding with automatic `gantry init --ssh` and agent-first `--json` diagnostics, `gantry why`/`explain`, `doctor --e2e`/`--drill`.
