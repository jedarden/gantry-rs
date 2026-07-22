# Design Decisions

## DD-1: PATH shim, not agent hooks

dcg integrates with each agent's hook system (Claude Code PreToolUse, Codex, Gemini CLI, …) because its job requires seeing *every* shell command. Gantry's job only requires owning a handful of binary names (`cargo`, later maybe `pytest`, `go`). A shim directory early in PATH achieves that for every caller — interactive shells, agents, scripts, Makefiles — with zero per-agent configuration and no dependency on any agent's hook API. The in-house predecessor proved this: NEEDLE workers inherited offloading purely via PATH, with no NEEDLE changes.

Mechanism: symlinks named after the target tool pointing at the `gantry` binary; argv[0] dispatch selects the tool profile (the rustup/volta/asdf pattern).

Consequence: gantry must resolve the *real* binary carefully (strip its own shim dir from PATH before lookup; explicit `real_binary` config override) to avoid exec-ing itself recursively.

## DD-2: Push to `refs/gantry/<sha>`, never branches

The predecessor script ran `git push origin HEAD` — which on 2026-07-19 pushed an unfinished local `main` to a public mirror-fed remote. For a public tool this is disqualifying: an offload tool must never mutate branch state as a side effect of running tests.

Gantry pushes the commit to a dedicated hidden ref: `git push <remote> <sha>:refs/gantry/<sha>`. Remote executors clone/fetch that ref by sha. Branch heads are untouched; refs are content-addressed so concurrent pushes can't conflict; periodic server-side cleanup of old `refs/gantry/*` is a documented maintenance note (they're cheap — the objects are usually already on the remote).

Config may name a dedicated `ci_remote` distinct from `origin` for setups where origin must not receive even hidden refs. Legacy `push = "branch"` mode exists only behind an explicit opt-in.

## DD-3: Cleanliness means `git status --porcelain` is empty

The predecessor used `git diff --quiet HEAD`, which misses **untracked files**. A brand-new test file would silently not exist remotely: tests "pass" against code that isn't what's on disk — the worst failure mode this tool can have, because it's invisible. Gate: `git status --porcelain` (respects .gitignore) must be empty, and `HEAD` must exist. Any output → local run with the reason printed.

Deliberate non-goal: syncing dirty state (rsync/`git stash create` tricks). Dirty trees run locally; that keeps the remote contract trivially reproducible ("a commit") and the security story clean.

## DD-4: The fallback ladder ends in a capped local run — but test failures are terminal

Two different kinds of "remote didn't work":

1. **Infrastructure failure** (push rejected, submit error, pod never scheduled, log stream lost, deadline exceeded before a verdict) → fall back to the capped local run. No test result was produced, so producing one locally is strictly better, and the cap keeps it safe.
2. **Test failure** (remote ran the suite, suite failed) → exit non-zero, full stop. Re-running locally would waste the machine the tool exists to protect, and risks masking a real failure with an environment-dependent local pass.

Every rung of the ladder logs why it was taken. "Silently skipped a test run" must be an unrepresentable state.

## DD-5: Backend abstraction with `command` as the universal backend

One trait: `submit(RunSpec) -> RunHandle`, `stream(&RunHandle)`, `wait(&RunHandle) -> Verdict`, `cancel(&RunHandle)`. Ship three implementations in this order:

- `command` — submit/logs/wait are user-supplied command templates with `{repo}`, `{rev}`, `{args}` substitution (proper argv-array substitution, not string splicing — see DD-7). This makes gantry adaptable to *any* CI system on day one and keeps the core honest about the abstraction.
- `argo` — first-class port of the in-house flow: create Workflow from a WorkflowTemplate with parameters, poll for pod, stream logs, poll `status.phase`. Authoritative verdict from workflow status, never from log capture (podGC deletes pods at completion).
- `ssh` (later) — for the no-Kubernetes audience.

The reference Argo `WorkflowTemplate` (fmt/clippy/check/test with optional sccache) ships in `contrib/argo/` as the documented remote-side contract, not as a hard dependency.

## DD-6: Rust, single static binary

- Fixes the predecessor's string-splicing problems structurally (YAML built by serde, argv arrays, no shell interpolation).
- Matches the ecosystem it serves and the local release pipeline pattern (Rust binary → tagged release).
- musl static build so `install.sh` works everywhere; no runtime deps beyond `git` and whatever the chosen backend needs (`kubectl` or SSH).

## DD-7: No shell/string splicing of user arguments — ever

The predecessor interpolated `TEST_ARGS` raw into a YAML heredoc: `cargo test -- "foo: bar"` could corrupt or alter the manifest. In gantry, user args travel as arrays end-to-end; backend payloads are serialized (serde_yaml/json); the `command` backend substitutes placeholders at the argv level. Remote side receives args as a single JSON-encoded parameter it decodes back to an array.

## DD-8: cargo-only interception in v1

The mechanism generalizes (`pytest`, `go test`, `npm test`), and the config schema is written tool-generically (`[tool.cargo]` tables) so extension isn't a rewrite. But v1 ships cargo-only: one tool profile, one reference backend contract, honest scope. Multi-tool is a v2 flag-day only for the docs.

## DD-9: Local resource cap is a feature, not just a fallback detail

`systemd-run --scope --user -p CPUQuota=200% -p MemoryMax=6G -p MemorySwapMax=0` (values config-driven) wraps local runs of intercepted commands — and optionally *all* passthrough cargo invocations (`cap_passthrough = true`, the in-house default). On a box shared by ~20 agents this is what stands between one `cargo test --release` and a dead machine; the predecessor's values have been production-proven under exactly that load. Where systemd user scopes are unavailable, degrade to plain exec (containers are usually already resource-bounded).
