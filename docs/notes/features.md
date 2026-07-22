# Features & Constraints

## The one-sentence product

A PATH shim that intercepts expensive build/test commands (cargo-first), gates on repo cleanliness, offloads to a pluggable remote executor with faithful log streaming and exit codes, and always degrades to a resource-capped local run — transparently to the caller.

## Must have (v1)

- **Transparent interception.** A shim named `cargo` sits ahead of the real cargo in PATH. Callers — humans, NEEDLE workers, Claude Code, any agent — run vanilla `cargo test` and get the right behavior. Zero caller-side changes, zero agent-hook configuration.
- **Interception policy.** Configurable list of intercepted subcommands (default: `test`). Everything else passes through to the real binary, optionally wrapped in the local resource cap.
- **Eligibility gate.** Remote execution only when: inside a git work tree, a configured remote exists, and the tree is clean **including untracked files** (`git status --porcelain`, not `git diff --quiet HEAD` — see design-decisions DD-3). Anything else → local capped run, with a one-line explanation of why.
- **Safe code sync.** Push the exact commit to a dedicated ref (`refs/gantry/<sha>`) — never implicitly push branches (DD-2; this exact footgun leaked an unfinished `main` in the predecessor script on 2026-07-19).
- **Pluggable backends** behind one trait:
  - `command` — universal escape hatch: user-supplied submit/logs/wait command templates. Makes gantry useful on day one to people with any CI system.
  - `argo` — first-class Argo Workflows backend (submit Workflow from a WorkflowTemplate, stream pod logs, poll phase). Reference `WorkflowTemplate` shipped in `contrib/`.
- **Local capped executor.** `systemd-run --scope --user -p CPUQuota=… -p MemoryMax=… -p MemorySwapMax=0`; limits configurable (default 200% / 6G, matching the proven in-house values). Falls back to plain `exec` where systemd user sessions are unavailable (containers).
- **Fallback ladder, never silent.** Every failure on the remote path (dirty tree, push rejected, submit error, pod never scheduled, log stream lost, timeout) lands on the capped local run — except a genuine remote **test failure**, which must propagate as a failure, never trigger a local retry (DD-4).
- **Exit-code fidelity.** Remote pass → 0; remote test failure → non-zero; infrastructure failure → fallback (and the fallback's exit code is the result). The caller can always trust the exit code.
- **Escape hatches.** `GANTRY_LOCAL=1` env var and `gantry off`/`gantry on` toggle; both bypass interception entirely.
- **`gantry doctor`.** Verifies PATH ordering (shim before real cargo), real-binary resolution, git state, backend reachability, systemd-run availability, config validity.
- **Structured logging.** Run records (repo, rev, args, backend, outcome, durations) appended to `~/.local/state/gantry/runs.jsonl`; human-readable `[gantry]`-prefixed status lines on stderr so agents' transcripts show what happened.

## Should have (v1.x)

- **In-flight dedup / join.** Two agents running `cargo test` on the same (repo, rev, args) join the same remote run instead of submitting twice. Keyed on content, coordinated via a lock/state dir. Multi-agent boxes hit this constantly.
- **`gantry run -- <cmd>`** — explicit offload of an arbitrary command without shimming.
- **Installer + uninstaller.** `install.sh` (curl-pipe, dcg-style): drops the binary, creates the shim symlink in a dir verified to precede the real toolchain in PATH, writes a starter config, runs `doctor`.
- **SSH backend.** `git push` a ref to the target host (or fetch from origin), run the command remotely under a cgroup scope, stream stdout. Serves the "I have a big desktop and a laptop" audience — no Kubernetes required.
- **Timeout/deadline config** per backend, with a clear "timed out, here's the run URL" message.

## Won't have (explicitly out of scope)

- **Per-agent hook integrations** (Claude Code hooks, Codex, Gemini CLI…). dcg needs them because it must see *every* shell command; gantry only needs to own a few binary names, and PATH shimming is simpler and agent-agnostic (DD-1).
- **Compilation caching/distribution.** That's sccache's job; gantry composes with it (the reference Argo template already uses sccache→S3). Gantry moves *where* the command runs, not *how* compilation is cached.
- **Bazel-style hermetic remote execution.** No input fingerprinting, no action graph. The unit of offload is "a commit plus a command line".
- **Windows.** Linux first; macOS local-fallback path (no systemd) uses plain exec until someone cares.
- **`cargo nextest` interception** in v1 — the in-house system deliberately excludes it (builder image may lack nextest); revisit once the reference image ships it.

## Constraints

- The shim must add **negligible latency** to non-intercepted commands (`cargo build`, `cargo check` run hundreds of times a day) — a few ms of argv dispatch, no git or network calls on the passthrough path.
- Must behave correctly when invoked concurrently by ~20 agents on one box (the NEEDLE fleet reality): no lock-file corruption, no interleaved log mangling, dedup rather than stampede.
- Must respect `~/.cargo/config.toml` quirks — notably a global `target-dir` redirect (in use locally: builds redirect to a shared `~/target/`) affects the local fallback's disk behavior but must not affect remote runs.
- Remote-side contract must tolerate `podGC: OnPodCompletion` (pods vanish at completion): the workflow's authoritative result must come from workflow status, not from having caught the logs.
- No secrets on the client beyond what the backend already requires (kubeconfig path, SSH alias). Gantry itself stores no credentials.
- Single static binary (musl) so the installer works on any distro.
