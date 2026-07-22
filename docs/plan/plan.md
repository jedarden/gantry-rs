# gantry — Application Plan

*Last updated: 2026-07-22. Revision history lives in git; decision provenance in `docs/notes/ideas-ledger.md`.*

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

## Terms & conventions

**Normative language:** MUST / MUST NOT are binding — violating them fails this spec. SHOULD means deviation requires a documented reason. MAY is truly optional.

**Glossary** (two uses of "gate" exist; they are different things):
- **shim** — the binary installed under the name `cargo`, ahead of the real toolchain in PATH.
- **intercepted / passthrough** — an invocation gantry may offload vs. one it forwards untouched (under the cap when `cap_passthrough`).
- **GitGate** — the *eligibility* checks (work tree, HEAD, remote, clean) deciding remote vs. local. Never called just "gate" in output.
- **quality gate** — an opt-in remote check beyond the caller's argv (clippy, fmt), always labeled `[gantry] gate:` and carrying verdict `gate_failure` when failed.
- **verdict** — the terminal classification of a run (`pass` / `test_failure` / `gate_failure` / `infra_failure` / `cancelled` / `superseded`). The exit code is derived from it, never vice versa.
- **Tier-0** — zero-config mode: cap-only, nothing goes remote.
- **backend / preset** — a `RemoteBackend` implementation vs. a shipped configuration of one (SSH is a preset of `command`).
- **fallback / degradation** — the loud, recorded switch to a capped local run after an infra failure or gate ineligibility.
- **epoch ref / lease** — `refs/gantry/<epoch>-<sha>` plus the client-side record that protects it from GC while a run is live.
- **attachment / waiter** — a process streaming/waiting on a run (originator or JoinTable joiner).

**Scope lock:** v1 scope is frozen as of 2026-07-22 (17 adopted mechanisms; see Resolved questions). New scope enters only via the ideation ledger → plan amendment → commit, *before* implementation of that item begins. In-flight scope additions are forbidden — this plan doubled in mechanism count across two productive ideation days precisely because the pipeline works; the freeze is what makes Phase 1 finishable.

## Acceptance scenarios

Named scenarios define "done." Each is independently verifiable; phase acceptance lists reference them.

**AS-1 Happy path (human).** A developer in a clean, pushed-up repo runs `cargo test`. → gantry pushes the epoch ref, submits, streams logs live, prints the verdict trailer. → Exit 0 on pass.
*Pass:* zero caller-side changes; logs stream within 30s of submit or a queue explanation prints; exit code equals what local `cargo test` would have produced.
*Fail:* any branch ref moved on the remote; silence >60s with no `[gantry]` line; exit code differs from the suite's true result.

**AS-2 Agent machine-mode (primary audience).** A NEEDLE worker (`GANTRY_AGENT=1`) runs `cargo test` on a wip branch; the suite has one failing test. → Remote runs, verdict `test_failure` with `failure_class: test-failure`. → The worker runs `gantry why --json` and branches on the failure class without parsing logs.
*Pass:* transcript contains the decision line, verdict trailer, and handle; `why --json` parses with a stable schema and names the failure class; the worker never needed a human.
*Fail:* worker must regex the log stream to learn what happened; JSON schema drifts between invocations; a gate failure is indistinguishable from a test failure.

**AS-3 Cluster outage under fleet load.** 20 agents run `cargo test` while the backend is unreachable. → Every run prints the infra reason, acquires a fallback slot (max 3 concurrent), and runs capped locally; the rest queue loudly.
*Pass:* box load stays bounded (slice cap holds); every run ends in a real verdict; runs.jsonl shows 20 intent+verdict pairs with `ran: local_after_infra`.
*Fail:* simultaneous uncapped local builds (the meltdown gantry exists to prevent); any run exits with neither verdict nor orphan record.

**AS-4 Dirty tree.** A developer with an uncommitted new test file runs `cargo test`. → GitGate blocks remote (untracked file detected), prints the reason and the exact two-command path to eligibility, runs capped locally.
*Pass:* the untracked file's tests actually ran (locally); reason `dirty` recorded; no ref pushed.
*Fail:* remote runs against code missing the new file (the predecessor's silent-gap bug); user must read docs to learn why it ran locally.

**AS-5 Gate failure attribution.** Gates enabled (fleet config); tests pass but clippy fails. → Exit non-zero with verdict `gate_failure`, output attributing the failure to `[gantry] gate: clippy`, tests reported green.
*Pass:* an agent reading `why --json` sees tests-passed-gate-failed and fixes lints, not tests.
*Fail:* gate failure presented as a test failure (misdirected agent work), or gates-off runs affected in any way.

**Success metrics:** *Performance* — passthrough <5ms p99; submit overhead (gate+push+submit, excluding queue) ≤5s p50. *Functionality* — AS-1..AS-5 pass. *Adoption* — one week of NEEDLE traffic with zero silent skips and ≥90% of eligible runs offloaded; bash pair deleted.

## Architecture

```
caller (human / agent / script)
   │  runs `cargo test …`
   ▼
[shim dir]/cargo ──symlink──► gantry binary
   │  argv[0]=cargo → tool profile "cargo"
   ├─ subcommand not intercepted ───────────────► LocalExecutor (capped) → real cargo
   ├─ GANTRY_LOCAL=1 / disabled / Tier-0 (no config) ──► LocalExecutor (capped) → real cargo
   ▼
DecisionEngine
   ├─ GitGate: work tree? remote? clean (porcelain)? HEAD?
   │     └─ any "no" → LocalExecutor (reason printed)
   ▼
RefPusher: git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>  (+ lease, + GC sweep)
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
- Dispatch on `argv[0]`: `cargo` → cargo tool profile; `gantry` → management CLI (full command set defined once in the “CLI surface” section below).
- Real-binary resolution: config override `real_binary`, else PATH lookup with gantry's own shim dir stripped (self-recursion guard: refuse to exec a path whose canonical target is the gantry binary).
- Passthrough path budget: no git/network/config-parse beyond a single mmap'd config read; target < 5 ms overhead.

### 2. DecisionEngine
- Intercept set from config (`[tool.cargo] intercept = ["test"]`).
- Tier-0 (zero-config): with no config file present or `backend = "none"`, gantry is a pure local cap-wrapper — interception still applies the cgroup cap and full RunLog, nothing goes remote, and a `[gantry]` line notes the tier at most once per hour (state-file timestamp) so transcripts see it without per-run noise. Install→value with zero setup; remote offload is an upgrade, not a prerequisite.
- Kill switches: `GANTRY_LOCAL=1`, `gantry off` (state file).
- Config-failure policy (resolves Q-7 via last-known-good): on parse failure, serve the persisted last-good config snapshot from the state dir and print an escalating `[gantry] config broken since <ts>, N runs degraded` banner on every intercepted run until fixed; with no snapshot, fail open to plain passthrough with the same banner. Behavior is never derived from a half-parsed file — a broken gantry must never block builds, and must never rot silently either.

### 3. GitGate
- Checks in order, each with a printed reason on failure: inside work tree → `HEAD` resolves → configured remote exists → `git status --porcelain` empty.
- All git interaction shells out to system `git` (no libgit2 — matches predecessor behavior, honors user's git config/credentials/hooks).

### 4. RefPusher
- `git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>` (`ci_remote` defaults to `origin`); the epoch prefix is the GC lever (see below).
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

- **`argo`** (v1): builds the Workflow manifest with serde (no string splicing), submits via `kubectl create -f -` (kubeconfig path from config; direct REST API is a later option), polls pods for streaming, polls `status.phase` for the verdict, falls back to reading `outputs.parameters.output` when podGC ate the logs. Ships `contrib/argo/gantry-verify-workflowtemplate.yml` — a genericized `rust-verify` (parameters: `repo`, `revision`, `args-json`, `builder-image`; sccache optional). Remote contract (resolves Q-2 as faithful-argv): the remote executes exactly the argv the caller typed; check/clippy run only as opt-in quality gates from trusted user config, reported as separately-labeled `[gantry] gate:` results, never conflated with the test verdict. A failing enabled gate yields verdict `gate_failure`: overall exit is non-zero (agents must see red), output and records attribute it to the named gate, and — like TestFailure — it never triggers a local retry. The template also emits a structured `verdict.json` output parameter — one versioned schema (`schema_version`, phase, exit code, oom, deadline from workflow node status, plus an optional `failure_class`: compile-error vs test-failure vs doctest vs harness-panic, derived from cargo's stable `--message-format json` stream) — so OOMKilled and deadline-exceeded classify as InfraFailure → fallback, never TestFailure, and agents branch on failure class without parsing logs. Consumers ignore unknown fields; an absent verdict.json degrades gracefully to exit-code-only interpretation (command templates). Parity preflight: builder images carry capability labels (`org.gantry.toolchain=…`); before submission gantry compares local rust-toolchain.toml and requested features against them — a mismatch is a loud local fallback (warn-only for unlabeled images), because a verdict from the wrong toolchain is a wrong answer delivered confidently.
- **`command`** (v1): three user-configured argv arrays (`submit`, `logs`, `wait`) with `{repo}` `{rev}` `{args_json}` `{handle}` placeholder substitution at the argv level; `submit`'s stdout is the handle; `wait`'s exit code is the verdict (0 pass, 1 test-failure, ≥2 infra-failure — documented contract).
- **SSH-first onboarding** (v1, flagship): not a third backend implementation — a shipped command-template *preset* plus `contrib/gantry-exec.sh` (~150-line POSIX executor that clones `refs/gantry/*` and runs the command), which doubles as executable documentation of the remote contract. "Remote = any spare SSH box" is the documented quickstart path; Argo is the "advanced" backend. A native `ssh` backend remains possible later only if the preset proves limiting.

### 6. LocalExecutor
- `systemd-run --scope --user -p CPUQuota={n}% -p MemoryMax={m} -p MemorySwapMax=0 -q -- <real> <args…>`; on failure to create a scope (no user session / container), plain `exec`.
- Applies to: intercepted-command fallbacks always; all passthrough invocations when `cap_passthrough = true` (default, matching predecessor).
- Fallback admission semaphore: capped local *fallback* runs additionally acquire a flock-based token file (default 3 concurrent per box) and queue with loud `[gantry] waiting for local slot (N ahead)` lines and a bounded wait — a remote outage under ~20 agents degrades as a serialized trickle, not a stampede that recreates the meltdown gantry exists to prevent. Kernel lock release means a SIGKILLed agent can never leak a slot.
- Boxwide slice: every gantry-spawned local run lands in one systemd user slice (`gantry.slice`) carrying a box-level *sum* cap (configurable, e.g. 12 CPU / 32G) alongside the per-run caps — the semaphore bounds the count, the slice bounds the total; 20 × "200%/6G each" can no longer add up past the machine. Degrades cleanly where slices are unavailable.
- Agent-priority queueing: `GANTRY_AGENT=1` (env convention — deliberately not TTY sniffing, since tmux-hosted agents have TTYs) orders the fallback-semaphore queue humans-first, agents-behind, with a printed triage tag (any future submission gating inherits the same ordering).

### 7. RunLog & UX
- stderr lines prefixed `[gantry]`: decision, workflow name/URL, fallback reason, verdict. Written for agent transcripts as much as humans.
- `~/.local/state/gantry/runs.jsonl`: write-ahead form — an OPEN intent record is appended *before* dispatch, and every exit path must append a matching terminal verdict record; `gantry doctor` flags orphaned OPEN records as lost runs, making a silently-skipped run structurally detectable rather than an absence of evidence. O_APPEND single-line writes for concurrency safety.
- Intent records capture all gate inputs (tree state, remote, kill-switch state, chosen backend) so `gantry why` can replay the decision truthfully after the fact.
- Flight recorder: every InfraFailure snapshots a redacted diagnostic bundle (config snapshot, git state, raw backend response, recent stderr) to `~/.local/state/gantry/crash/<run-id>/`; `gantry report <run-id>` prints or packages it. Infra flakes become post-mortem-able artifacts instead of archaeology; the redaction list is load-bearing and conservative by default.

### 8. doctor / installer
- `gantry doctor`: shim-before-real PATH ordering; real binary resolves and is not gantry; config parses; backend preflight (`kubectl` reachable / template exists, or command-backend binaries exist); systemd-run scope creation probe; git identity present; orphaned intent records reported as lost runs.
- `gantry doctor --e2e`: canary run — pushes a tiny known-good fixture ref through the real RefPusher and backend and asserts the expected pass verdict round-trips. One command answers "is the pipeline actually working" on the operator's schedule, not at 9am when 20 agents degrade. Mechanism: pushes the current repo's HEAD as a normal gantry ref but overrides the argv with a trivial command (`cargo --version`), exercising push → clone → contract → verdict without running a real suite; `gantry init --ssh` runs it as its final step.
- `gantry doctor --drill`: fault-injection fire drill — injects a synthetic InfraFailure through a drill-scoped hook (never honored outside a drill) and asserts the entire loud-degrade chain fires: banner, intent/verdict records, semaphore-gated capped local run, correct exit code. The fallback path is gantry's core safety promise and otherwise only executes when things are already on fire.
- `install.sh`: detect platform → fetch release binary → install to `~/.local/bin/gantry` → create shim symlink(s) in a configured shim dir → verify PATH ordering (instruct if wrong) → write starter config → run `doctor`. `gantry uninstall` reverses it.
- Onboarding must be smooth and automatic: `gantry init --ssh user@host` performs the whole SSH-first setup — verifies git+cargo on the target, writes the command-template preset config, and finishes with `doctor --e2e`. Install to first remote run in ~10 minutes, no Kubernetes required.

### 9. JoinTable (v1.x)
- Key: `(remote_url, sha, tool, args)` hash. A flock'd state dir maps in-flight keys → RunHandle; a second invocation attaches to the existing handle (stream + wait) instead of resubmitting.

### 10. Ledger intelligence (v1.x)
One verdict-history module over runs.jsonl powering three features:
- **Memoization (opt-in, never default):** an identical `(tree-hash, args, toolchain, image digest)` with a terminal PASS verdict returns the recorded verdict instantly with a `[gantry] cached` line and a `--fresh` escape. Keyed on tree hash, not commit sha, so rebases/amends with identical trees still hit. Failures are never memoized.
- **Supersede-on-new-commit:** a newer sha submitted for the same (repo, args) cancels the older still-running sibling with an explicit `superseded` terminal record — recorded, never silent — reclaiming cluster minutes from fast-iterating agents. Supersede never yanks a watched run: it applies only when the older run has zero live attachments (originator gone, no JoinTable waiters); otherwise the newer sha simply runs alongside.
- **Flake flagging:** the same tree + args with flipping verdicts marks runs `flaky-suspect` in runs.jsonl and the stderr summary.

### Proposed module layout

```
src/main.rs            argv[0] dispatch only
src/shim.rs            passthrough fast path (<5ms budget lives here)
src/decision.rs        intercept set, Tier-0, kill switches
src/gate.rs            GitGate
src/refs.rs            RefPusher: epoch refs, leases, GC sweep
src/backend/mod.rs     RemoteBackend trait + verdict.json parsing
src/backend/argo.rs    kubectl-based Argo backend
src/backend/command.rs command-template backend (SSH preset = config + contrib script)
src/local.rs           LocalExecutor: scope, slice, semaphore, priority
src/runlog.rs          write-ahead ledger, flight recorder
src/config.rs          layering, last-known-good, trust boundary
src/cli/               doctor, why, explain, init, quickcheck, report, status…
contrib/argo/          reference WorkflowTemplate
contrib/gantry-exec.sh shell reference executor
tests/properties/      argv round-trip, ref-name fuzzing
tests/stampede/        20-way concurrency harness
tests/e2e/             drill + canary fixtures
```

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
backend    = "none"          # none = Tier-0 cap-only (zero-config default) | argo | command
ci_remote  = "origin"
push_mode  = "ref"           # ref | branch (legacy, warns)
deadline_minutes = 40

[remote.argo]
kubeconfig = "~/.kube/config"
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
```

`cwd_rel` is the caller's directory relative to the repo root; remote executors `cd` into it before running the argv, so workspace-member invocations behave identically remote and local.

```rust
// runs.jsonl — write-ahead: one OPEN intent + one terminal record per invocation
{ "rec": "intent", "schema_version": 1, "run_id": "01J…", "ts": …,
  "tool": "cargo", "args": ["test","--","--nocapture"], "repo": "…", "sha": "…", "cwd_rel": ".",
  "gate": { "worktree": true, "head": true, "remote": true, "clean": true },
  "decision": "remote|local", "reason": "clean|dirty|no_remote|tier0|disabled|env",
  "backend": "argo|command|none" }
{ "rec": "verdict", "schema_version": 1, "run_id": "01J…", "ts": …,
  "verdict": "pass|test_failure|gate_failure|infra_failure|superseded|cancelled",
  "ran": "remote|local|local_after_infra", "exit_code": 0, "handle": "gantry-x7k2p",
  "durations_ms": { "gate": 40, "push": 900, "queue": 12000, "run": 341000 } }
```

### Verdict semantics (the load-bearing enum)

| Verdict | Meaning | Action |
|---|---|---|
| `Pass` | remote ran suite, passed | exit 0 |
| `TestFailure` | remote ran suite, failed | exit non-zero; **no local retry** |
| `InfraFailure` | no verdict produced (push/submit/schedule/stream/deadline) | capped local run; its exit code wins |

OOMKilled pods and deadline-exceeded workflows are `InfraFailure` by definition (via the remote contract's `verdict.json`): an infrastructure cap firing says nothing about the code, and reporting it as a test failure would fake a result.

Terminal-record values extend the ladder: `gate_failure` (an enabled quality gate failed while tests passed — exit non-zero, attribution to the named gate, no local retry), `cancelled` (user Ctrl-C), and `superseded` (v1.x, see Ledger intelligence). `verdict` applies to local runs too (`ran: "local"`); an infra fallback records the final local outcome as `ran: "local_after_infra"`.

### Versioning & compatibility

Every machine-consumed surface carries an explicit version; unknown fields are always ignored (additive evolution is free, removal/retyping is a major bump):

- **runs.jsonl** — `schema_version` on every record. Readers MUST tolerate newer minor records; `gantry doctor` flags a ledger written by a newer major.
- **verdict.json** — `schema_version`; absent file = exit-code-only contract. Optional fields (`failure_class`) are exactly that.
- **`--json` CLI output** (`why`/`explain`/`status`) — top-level `schema_version`; the JSON surface is the primary interface, human text is a rendering of it.
- **Remote contract** — the client sends `contract_version` as a workflow parameter; the template echoes it in verdict.json. A mismatch the client can't interpret = InfraFailure with an explicit "contract drift" message, never a misread verdict. Template upgrades are blue-green: `gantry-verify` versions coexist and the client config pins one.
- **Config** — unknown keys warn (never error); renamed keys keep a one-major deprecation alias; the last-known-good snapshot stores the schema version it parsed under.
- **Backward-compat stance for the predecessor:** cap values and nextest non-interception match the bash pair exactly; behavioral differences (porcelain gate, epoch refs, OOM classification) are deliberate fixes, listed in the migration notes — there is no bug-for-bug mode.

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
gantry quickcheck           # 30s no-backend sanity: shim resolves, cap works, git ok (Tier-0 proof)
gantry report <run-id>      # print/package the flight-recorder bundle for an InfraFailure
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
| log stream lost mid-run | infra (recoverable) | keep waiting on status; fetch output param at end |
| client deadline (`deadline_minutes`) exceeded | infra | print handle URL, local capped run |
| remote suite fails | test | exit non-zero, stop |
| enabled quality gate fails (tests pass) | gate_failure | exit non-zero, attributed to the named gate; no local retry |
| remote pod OOMKilled / workflow `activeDeadlineSeconds` exceeded | infra | `verdict.json` classifies as InfraFailure → capped local run — never reported as a test failure |
| gantry process killed mid-run | self | orphaned OPEN intent in runs.jsonl; `gantry doctor` reports it as a lost run |
| remote outage → mass fallback | env | admission semaphore caps concurrent local runs (default 3), loud queue-position lines |
| Ctrl-C during remote run | user | propagate `cancel()`, exit 130 |
| gantry config broken/missing | self | serve last-known-good snapshot + escalating banner; plain passthrough if no snapshot (never silent) |
| systemd-run unavailable | env | plain exec (documented: containers self-limit) |
| gantry binary deleted but shim remains | self | shim *is* the binary (symlink) — impossible state; doctor checks dangling links |

## Edge case catalog

**EC-01 Linked git worktrees (load-bearing: NEEDLE workers share worktrees).** All GitGate checks and pushes operate on the *worktree's* HEAD and index. JoinTable and memoization keys use the common git dir (`git rev-parse --git-common-dir`) + sha, so two worktrees of one repo dedup correctly. A stampede-test variant MUST run from two worktrees of the same repo.

**EC-02 Detached HEAD.** Eligible — a sha is a sha. The decision line notes `(detached)`; nothing else differs.

**EC-03 Submodules.** If `.gitmodules` exists, v1 falls back locally with reason `submodules` (the remote clone contract doesn't recurse yet). Loud, honest, and cheap; recursion is a template upgrade later.

**EC-04 Shallow local clone.** Push of the epoch ref may fail against a shallow history → ordinary push InfraFailure → capped local run. No special handling; the reason line names the likely cause.

**EC-05 Huge repo / slow status.** `git status --porcelain` on a monorepo can take seconds. Gate duration is recorded (`durations_ms.gate`) and a `[gantry] slow gate (Ns) — consider .gitignore hygiene` hint prints past 2s. The gate is never skipped for speed.

**EC-06 Pathological argv.** NULs are impossible in argv; newlines/quotes/100KB args MUST round-trip byte-exact (property test, tests/properties/). Args are never shell-interpolated anywhere (S-4).

**EC-07 Cargo aliases and bare `cargo`.** Interception matches argv[1] exactly against the intercept set. Aliases expanding to `test` inside real cargo are NOT intercepted (documented limitation — gantry never parses cargo config to chase aliases).

**EC-08 State dir unwritable / disk full.** Never blocks the build: run proceeds with in-memory records, one warning line, and `doctor` reports the unwritable state dir. A full disk MUST NOT turn into a missing verdict or a crash.

**EC-09 Same sha pushed by two boxes in the same epoch window.** Ref name collision is benign (same content); the push is verify-then-skip. Cross-box lease files don't exist — each box GCs only refs it created (lease records are local).

## Invariants (CI-enforced, not prose)

- **INV-1 No silent skip:** every invocation ends in a terminal record or a detectable orphaned intent. (SIGKILL test in Phase 1 acceptance.)
- **INV-2 No branch mutation:** gantry never moves a branch ref; asserted via `git ls-remote` diff in e2e tests.
- **INV-3 Exit-code fidelity:** the caller's exit code always reflects the verdict; verdicts derive from status APIs, never from parsing log text.
- **INV-4 Passthrough budget:** <5ms p99 (hyperfine in CI; regression = build failure).
- **INV-5 Argv round-trip:** byte-exact through shim → JSON → remote argv (property test).
- **INV-6 Network confinement:** no connections except the configured backend and git remote (S-7 test).
- **INV-7 No local retry of real failures:** `test_failure` and `gate_failure` never trigger a local rerun.

## Security considerations

- **S-1 no implicit branch pushes** (DD-2) — the predecessor's worst incident class.
- **S-2 repo config is untrusted**: `.gantry.toml` cannot alter push targets, backends' credentials, or command templates. Only user/system config can define executable command templates.
- **S-3 no credential storage**: backends use ambient credentials (kubeconfig file path, SSH agent, git credential helpers). Gantry never reads/writes tokens.
- **S-4 arg hygiene** (DD-7): args as arrays end-to-end; remote receives `args_json`; no shell interpolation anywhere.
- **S-5 log hygiene**: run records store the remote URL as given but never credentials embedded in URLs (strip userinfo before logging).
- **S-6 published threat model**: `docs/notes/threat-model.md` states what gantry can and cannot touch — ref-leak surface, untrusted repo config, credential posture — and ships alongside the public README.
- **S-7 no-phone-home, as a test**: an integration test asserts the binary opens no network connections except the configured backend and the git remote; the pledge is CI-enforced, not prose.

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| R1 | Phase 1 scope exceeds estimate (17 adopted mechanisms) | High | High | Walking skeleton + 1a/1b split below; scope lock; 1b is the cut list if 1a slips |
| R2 | Epoch-ref naming vs content-addressing tension surfaces late | Medium | High | Design note is a Phase 1a *entry* deliverable, settled before RefPusher is built |
| R3 | Cold remote builds slower than local (sccache cold/absent) → adoption stalls | Medium | High | `doctor --e2e` measures round-trip; expectations documented; Tier-0 keeps the tool valuable regardless |
| R4 | kubectl dependency/version skew breaks argo backend | Medium | Medium | doctor preflight pins minimum kubectl; REST client remains the stretch escape |
| R5 | No systemd on target (containers, macOS) weakens caps | Medium | Medium | Documented degrade to plain exec; Q-5 open; doctor reports active cap tier |
| R6 | Argo output-param cap truncates failure tails | Medium | Medium | Q-6 open; watermarked truncation; artifact channel benched in ledger |
| R7 | Shared-worktree fleet semantics diverge from single-checkout assumptions | Medium | High | EC-01 + two-worktree stampede variant + Phase 3 fleet-cycle audit |
| R8 | `refs/gantry/*` propagate through server-side push mirrors to public GitHub | Medium | High | Interim: docs recommend a dedicated `ci_remote` for mirrored repos; mirror-leak guard benched (owner-skipped) |
| R9 | flock semantics unreliable on network filesystems | Low | Medium | State dir MUST be local disk; doctor verifies |

## Quality gates (definition of done)

`rust-toolchain.toml` is pinned at the repo root from the first commit of Phase 0.5. No phase completes — and no release ships — unless all of the following pass **on the same commit**:

1. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
2. `cargo test` (unit + property + stampede suites relevant to the phase)
3. hyperfine passthrough budget (INV-4) within target
4. The phase's acceptance list green
5. From Phase 3 on: no-phone-home test (INV-6) green in CI

A regression in any gate is a build failure, not an advisory. Gantry's own CI dogfoods the remote contract as soon as Phase 2 lands.

## Implementation phases

Each phase names its entry criteria (the prior phase's quality gates) and a rough size estimate — honesty devices, not commitments.

### Phase 0 — Repo, docs, contract ✅
Scaffold, research, features, decisions, plan.

### Phase 0.5 — Walking skeleton (~800 LOC)
**Goal:** prove the wiring end-to-end; everything may be crude, panics allowed. Shim dispatch on argv[0] → GitGate happy path → epoch-ref push → `command` backend running a bash executor on the same box → verdict → correct exit code. One hardcoded config; no semaphore, no doctor.
**Exit criterion:** one real `cargo test` round-trips through the shim to a local "remote" and back with the right exit code, for both a passing and a failing suite.

### Phase 1a — Core loop, hardened (~2,500 LOC)
**Entry:** skeleton exit criterion met; epoch-vs-content-addressing design note written and settled.
Config layering + last-known-good + trust boundary, full GitGate with reasons, RefPusher with leases + GC sweep, `argo` backend, complete verdict ladder (incl. `gate_failure`), write-ahead RunLog with gate-input capture, `GANTRY_LOCAL`/`on`/`off`, `doctor` core checks.
**Acceptance (AS-1, AS-4):** on a clean repo, `cargo test` runs remotely with streamed logs and correct exit codes for pass/fail; dirty repo falls back with reason; pod-never-scheduled falls back locally (not exit 1); `cargo test -- "weird: [args]"` round-trips intact (property suite, INV-5); passthrough overhead < 5 ms (hyperfine, INV-4); no branch ref moves on the remote (`git ls-remote` assert, INV-2); SIGKILLing gantry mid-run leaves an orphaned intent that `doctor` reports (INV-1); a corrupted config file produces last-known-good behavior + banner, never silence.

### Phase 1b — Safety rails & explainability (~2,000 LOC)
**Entry:** Phase 1a quality gates green.
Fallback admission semaphore, boxwide `gantry.slice` sum cap, `GANTRY_AGENT` priority queueing, Tier-0 zero-config mode + `gantry quickcheck`, flight-recorder bundles + `gantry report`, `gantry why`/`explain` (+ `--json`).
**Acceptance (AS-2, AS-3):** 20 simultaneous fallback invocations serialize through the semaphore (stampede test, including the two-worktree variant per EC-01); combined resource use of N concurrent capped runs stays inside the slice cap; a `GANTRY_AGENT=1` run queues behind an interactive one; a fresh install with zero config caps a local `cargo test` and passes `quickcheck` with no backend; an InfraFailure leaves a readable `gantry report` bundle; `why --json` validates against its published schema.

### Phase 2 — Remote contract & template (~1,200 LOC + YAML/shell)
**Entry:** Phase 1b quality gates green.
`contrib/argo/gantry-verify-workflowtemplate.yml` (genericized rust-verify: `args-json` param, ref-fetch clone of `refs/gantry/*`, optional sccache, faithful-argv default with opt-in labeled gates, versioned `verdict.json` output incl. failure taxonomy via `--message-format json`), `contrib/gantry-exec.sh` shell reference executor, output-param log recovery, deadline config, Ctrl-C cancel, refs GC sweep, `doctor --e2e`/`--drill`, image capability labels + parity preflight, contract-version handshake.
**Acceptance:** template applied on iad-ci; end-to-end run against a real repo; killing the client mid-run cancels the workflow; podGC'd fast run still prints full log from output param; an OOM-fixture suite classifies as InfraFailure and falls back (never "tests failed"); clippy findings appear as `[gantry] gate:` lines without failing a plain `cargo test`; stale refs reach zero at steady state; `doctor --drill` proves the full degrade chain end-to-end; compile-error and test-failure fixtures produce distinct verdict classes agents can branch on; a toolchain-mismatched image is refused with a loud local fallback (warn-only when unlabeled).

### Phase 3 — Onboarding, packaging & migration (mostly packaging/docs; ~600 LOC)
**Entry:** Phase 2 quality gates green, template live on the reference cluster.
musl static build; CI via Argo (`gantry-ci` WorkflowTemplate → tagged release, following the forge-ci/needle-ci pattern); `install.sh` + `gantry uninstall`; **SSH-first onboarding as the flagship quickstart** — `gantry init --ssh user@host` performs the whole setup automatically (verify target, write preset config, finish with `doctor --e2e`) and the README leads with it, documenting Argo as the advanced backend; agent-first `--json` diagnostics (`why`/`explain`/`status`) ship here so both humans and fleet agents can self-serve when something degrades; `docs/notes/threat-model.md` published with the README rewrite and the no-phone-home integration test lands in CI; README rewritten for external users, leading with Tier-0 (works with zero config) → SSH (10 minutes) → Argo (advanced).
**Migration:** replace `~/.local/bin/cargo` + `cargo-remote` on ex44 and lab with gantry shims; config reproduces current behavior (argo backend → `rust-verify` initially); NEEDLE fleet inherits via PATH with zero changes; keep bash scripts as `.bak` for one week; watch `runs.jsonl` across a fleet cycle.
**Acceptance:** one week of NEEDLE production traffic with no silently-skipped runs (audit runs.jsonl vs worker transcripts) and no branch pushes originating from gantry; curl-pipe install on a fresh box reaches a passing `quickcheck` with zero config; `gantry init --ssh` against a clean host ends with `doctor --e2e` green; the no-phone-home test is green in CI.

### Phase 4 — Fleet features (v1.x)
**Entry:** Phase 3 migration acceptance met (fleet is on gantry).
JoinTable dedup, ledger intelligence (opt-in memoization, supersede-on-new-commit, flake flagging), `gantry run`, `gantry status`, crates.io publish (name pending — open question Q-1). Native `ssh` backend dropped from this phase — superseded by the Phase 3 SSH-first preset; revisit only if the preset proves limiting.
**Acceptance:** two concurrent identical `cargo test` invocations produce one workflow, both callers get the verdict; a memoized re-run returns the cached verdict in <1s with the `[gantry] cached` line and `--fresh` forces execution; superseding cancels an unattached older run with a `superseded` terminal record; a verdict flip at identical tree is flagged `flaky-suspect`.

### Phase 5 — Stretch
Multi-tool profiles (`pytest`, `go test`) behind the same config schema; nextest interception + `--partition` sharding across pods; backend REST mode for Argo (drop kubectl dependency).

## Open questions

- **Q-1 crates.io name.** Is bare `gantry` registered/squatted? Check at publish time; fallbacks `gantry-rs` (matches the repo) / `gantry-shim`. Repo name stays `gantry-rs` regardless; binary stays `gantry`.
- **Q-4 `builder-image` selection.** A repo-chosen image is remote code execution with the CI pod's credentials (sccache keys, clone tokens) — an arbitrary image named in `.gantry.toml` would gut S-2. Leading candidate: repo config may only select images matching a trusted-config allowlist (registry/digest prefixes); a non-matching image is ignored with a loud `[gantry]` line and the default is used. Settle before Phase 2 template work.
- **Q-5 macOS local cap.** No systemd; `ulimit`/`taskpolicy` are weak substitutes. Ship uncapped-with-warning, or refuse to `cap_passthrough` on macOS?
- **Q-6 output-param size limits.** Argo output parameters have size caps (~256KB default); huge test logs will truncate. Artifact-based log capture (S3) as the durable channel?
## Resolved questions (2026-07-21 ideation run — full record in docs/notes/ideas-ledger.md)

- **Q-2 → faithful-argv.** The remote executes exactly the argv the caller typed; check/clippy are opt-in gates from trusted user config, reported as separately-labeled `[gantry] gate:` results. The in-house config keeps the gates on.
- **Q-3 → lease-based GC.** Epoch-named refs (`refs/gantry/<epoch>-<sha>`) plus client-side leases; every push opportunistically sweeps expired, terminal refs. No cron, no daemon; sweeps can never race an in-flight clone.
- **Q-7 → last-known-good.** Broken config serves the persisted last-good snapshot with an escalating banner (plain passthrough + banner if no snapshot exists). Never silent, never blocking, never derived from a half-parsed file.

Also adopted in that run (finalists 1–9 of 10; finalist 10, the demo/exit-code-oracle fixture, was declined): verdict.json infra-classification, write-ahead intent ledger, fallback admission semaphore, SSH-first onboarding with automatic `gantry init --ssh` and agent-first `--json` diagnostics, `gantry why`/`explain`, `doctor --e2e`/`--drill`.

Run 2 (2026-07-22) adopted: Tier-0 zero-config cap-only mode + `quickcheck`, ledger intelligence (v1.x), parity preflight with image capability labels, boxwide `gantry.slice` sum cap, `GANTRY_AGENT` priority convention, verdict.json v2 failure taxonomy, flight-recorder crash bundles + `gantry report`, threat-model doc + CI-enforced no-phone-home pledge. Declined: dropping the native Argo client from v1 (owner: Argo is the present usage pattern — it stays first-class). Skipped for now: mirror-leak guard (benched in the ledger).
