# Phase 3 migration runbook — bash cargo shim → gantry, both dev boxes

**Status: DRAFT — STOP. Nothing in this document has been executed on any box.**

This runbook was prepared by bead `bf-3c8` (Phase 3, plan §"Implementation phases →
Phase 3 → Migration"). The bead's instruction is explicit: *prepare the runbook and
STOP for explicit human sign-off; do not execute on production boxes autonomously.*
Every step below is written to be executed **after** the sign-off block (§10) is
completed by the operator. An agent handed this file may execute it only when that
block is filled in, and only on boxes the sign-off names.

Boxes in scope (§2.3): **lab** (this box) and **codinghome** — the two boxes where
the bash pair is deployed today.

Reference material: `docs/plan/plan.md` (Phase 3), `docs/research/in-house-implementation.md`,
`docs/notes/threat-model.md`, `install.sh` (repo root on `main` — merged via bf-3qm; not
necessarily present on this working branch), `contrib/argo/gantry-verify-workflowtemplate.yml`.

---

## 1. Scope

**In scope:** replacing `~/.local/bin/cargo` + `~/.local/bin/cargo-remote` on lab and
codinghome with the gantry shim (release binary + `cargo` shim in `~/.local/bin`, user
config reproducing current behavior, argo backend → `rust-verify` on iad-ci); one-week
`.bak` retention; the soak-week audit of `runs.jsonl` for silent skips (INV-1) and
branch pushes (INV-2).

**Out of scope (this runbook does not do these):** executing anything on the boxes
(§10 gate); NEEDLE worker/fleet-policy changes (the fleet inherits via PATH with zero
changes by design); the ex44 box named in plan.md — see §2.3; the Tier-0/curl-pipe
fresh-box acceptance line from plan Phase 3 (blocked by the Forgejo anonymous-access
login wall, tracked as P0-b, §5).

**Success criteria** (plan Phase 3 acceptance, restated operationally):
one week of NEEDLE production traffic after cutover with (a) no silently-skipped runs
— every `cargo test` invocation ends in a terminal `runs.jsonl` record or a
doctor-detectable orphan (INV-1); (b) no branch pushes originating from gantry
(INV-2); (c) exit-code fidelity preserved on pass and fail; (d) both boxes pass
`gantry doctor` and `gantry doctor --e2e` at all times during the soak.

## 2. Ground truth — the bash pair today

### 2.1 Files and behavior

Tracked copies of the deployed pair live in the operator dotfiles ("this directory"
per their headers); the deployed paths are what the migration replaces.

`~/.local/bin/cargo` (wrapper), in PATH order **ahead of** `~/.cargo/bin/cargo`:

1. Per-repo `CARGO_TARGET_DIR=/build/<repo>` allocation — active only on hosts with a
   `/build` directory (codinghome; lab has none, script leaves such hosts alone).
   Load-bearing since needle-d6b685b4: a shared target dir once produced 190 dirs /
   830G and filled the codinghome-class disk to 100%. Also **refuses** a
   `--target-dir`/`CARGO_TARGET_DIR` outside `/build/<repo>/` (exit 2).
2. Intercepts **only** subcommand `test`, and only inside a git repo with an `origin`
   remote → delegates to `cargo-remote`. Everything else runs capped-local.
3. Capped local path: `systemd-run --scope --user --slice="$(current_slice)"`
   `-p CPUQuota=200% -p MemoryMax=6G -p MemorySwapMax=0 -p RuntimeMaxSec=14400`;
   plain `exec` when systemd-run is unavailable. `--slice` places the scope in the
   **caller's** slice (needle.slice for fleet workers, app.slice interactive) —
   claudego-4746d945. `RuntimeMaxSec=14400` reaps hung scopes after 4h —
   needle-3d5c65d8, after the 2026-09-03 lab incident (five cargo-test binaries in
   unbounded scopes for 14–24 days).

`~/.local/bin/cargo-remote` (remote path, invoked by the wrapper for `cargo test`):

1. Falls back capped-local when: no `origin` remote; **tracked** uncommitted changes
   (`git diff --quiet HEAD` — untracked files do **not** trigger fallback, so a
   remote run can execute against a clone that lacks them: the silent-gap class
   AS-4 exists in this pair); ref push failure; submit failure.
2. **Pushes the current branch**: `git push origin HEAD` before every submit. Every
   clean-tree `cargo test` on either box moves (or confirms) the caller's branch on
   origin — the S-1 incident class gantry exists to eliminate, and also the mechanism
   by which NEEDLE wip branches have historically reached origin as a side effect
   (§4.2, watch-item W2).
3. Submits a Workflow to iad-ci (`kubeconfig` default `~/.kube/iad-ci.kubeconfig`,
   namespace `argo-workflows`, `generateName: cargo-remote-`, templateRef
   **`rust-verify`**, parameters `repo`, `revision`, `test-args` — args passed as a
   shell-word-split string, so quoted args with spaces do not round-trip).
4. Polls for a pod ≤3 min; **if no pod ever starts it exits 1 with no local run**
   (the failure plan's table explicitly corrects); streams `kubectl logs -f`; polls
   the workflow phase ≤35 min beyond the log stream; timeout → exit 1. On
   `Succeeded` → 0, `Failed|Error` → 1. No verdict records are kept anywhere: if
   logs are lost, the result is archaeology in the Argo UI (workflows TTL-reap after
   30 min success / 2 h failure).

The iad-ci `rust-verify` template (declarative-config
`k8s/iad-ci/argo-workflows/rust-verify-workflowtemplate.yml`, live 39d) runs, in one
container: `cargo check --all-targets` (fail-fast) → `cargo clippy --all-targets --
-D warnings` → `cargo test $TEST_ARGS`, all folded into one exit code, with
`activeDeadlineSeconds: 1800` on the verify step, `7200` workflow-level, and an
onExit GitHub commit-status post. Its declared parameters are `repo`, `revision`,
`test-args`, `builder-image`, `github-repo` — **not** `args-json`/`contract-version`
(§5 P1).

### 2.2 Deploy and drift infrastructure

- Deployed by `fleet/lab/apply-lab-fleet.sh --install-cargo-wrapper` (operator
  dotfiles). The migration either adds a sibling verb to that script or is executed
  as the manual steps in §6 — operator's choice at sign-off (D6).
- Drift is watched by `wrapper-drift.timer` against the deployed copies. Once
  `~/.local/bin/cargo` is a gantry shim, the drift watcher will fire constantly.
  **Pause/retire it during cutover** (§6 step 2) or the soak week drowns in false
  drift alerts.

### 2.3 Box list

plan.md (2026-07-22) says "ex44 and lab"; the bash pair's own headers and deploy
script say **lab and codinghome**. This runbook targets lab + codinghome and treats
"ex44" as a stale reference. The final box list is a sign-off item (D6).

## 3. Target state (per box)

```
~/.local/bin/gantry                     the release binary (static musl)
~/.local/bin/cargo                      shim → gantry binary (symlink or copy, per install.sh)
~/.local/bin/cargo.pre-gantry.bak       the bash wrapper, kept 1 week (§9)
~/.local/bin/cargo-remote.pre-gantry.bak  dead once nothing execs it; kept 1 week (§9)
~/.config/gantry/config.toml            behavior-reproducing config (Appendix A)
~/.local/state/gantry/                  runs.jsonl ledger, state.toml kill switch,
                                        LKG config snapshot, crash bundles
~/.config/systemd/user/gantry.slice     boxwide sum cap, provisioned by the binary
                                        on first capped run (12 CPU / 32G defaults)
```

PATH order is already correct on both boxes (`~/.local/bin` before `~/.cargo/bin`);
gantry resolves the real toolchain by stripping its own shim dir from PATH, with a
self-recursion guard.

## 4. Behavior parity

### 4.1 What carries over unchanged

| Aspect | bash pair | gantry |
|---|---|---|
| Intercepted subcommand | `test` only | `intercept = ["test"]` (Appendix A) |
| Per-run caps | 200% CPU / 6G / swap 0 | defaults identical (200 / "6G" / swap 0) |
| Passthrough capping | always capped | `cap_passthrough = true` (default) |
| systemd-run unavailable | plain exec | plain exec, loud one-line degrade |
| Dirty tree → local | tracked changes | porcelain (superset, §4.2) |
| No remote → local | yes | yes |
| Push failure → local | yes | yes (InfraFailure) |
| nextest | not intercepted (local capped) | not intercepted |
| Remote cluster/template | iad-ci `rust-verify` | same template pinned in config |
| Workflow log loss | Argo UI archaeology | `outputs.parameters.output` recovery (`output`/`result` params exist on rust-verify) |

### 4.2 Deliberate changes (improvements — do not "fix" these back)

1. **No branch pushes (INV-2 / S-1).** Gantry pushes `refs/gantry/<epoch>-<sha>`
   only; `push_mode = "ref"` is the pinned config. `cargo test` will never again
   move `refs/heads/*` on origin. This kills the bash pair's worst incident class —
   and removes an accidental feature: see watch-item W2.
2. **Untracked files now force local.** The bash pair's `git diff --quiet HEAD`
   missed untracked files, so a remote run could silently execute *without* them
   (false green on a new untracked test file). Gantry's porcelain gate runs such
   trees locally, where the untracked tests actually execute (AS-4). Soak effect:
   some runs that used to go remote will now be local; the `[gantry]` reason line
   names `dirty`.
3. **Detached HEAD is eligible (EC-02).** The bash pair's remote path works from any
   repo, but detached-HEAD worktrees commonly hit "uncommitted changes"/push
   confusion; gantry treats a sha as a sha. Expect detached-worktree runs to start
   offloading where they were local before.
4. **No more exit-1-on-infra.** Pod-never-scheduled, submit failure, and client
   deadline all classify `infra_failure` → **capped local run** with a printed
   reason, instead of the bash pair's bare `exit 1`. A worker sees a real test
   result either way (plan failure-mode table).
5. **Argv fidelity (INV-5).** `test-args` word-splitting is gone; args travel as a
   JSON array and round-trip byte-exact. Quoted args now behave remotely as they
   always did locally.
6. **Auditability.** Every invocation writes an OPEN intent before dispatch and a
   terminal verdict after (write-ahead, `~/.local/state/gantry/runs.jsonl`); `gantry
   why/status --json`, `doctor` orphan detection, and flight-recorder bundles for
   infra failures replace Argo-UI archaeology (workflows TTL-reap; the ledger does
   not).
7. **Verdict ladder.** Exit codes derive from workflow phase (and, later with
   `gantry-verify`, verdict.json), with infra never reported as test failure.

### 4.3 Known gaps needing a decision before cutover

These are the honest deltas where gantry does **not** reproduce the bash pair today.
Each has a decision ID; all are sign-off items (§10).

- **D1 — template contract mismatch (blocking).** Gantry's argo submit always sends
  parameters `repo`, `revision`, `args-json`, `contract-version` (+ `builder-image`
  when configured). Live `rust-verify` declares `repo`, `revision`, `test-args`,
  `builder-image`, `github-repo`. Argo rejects a submission supplying parameters the
  template does not define, so **`template = "rust-verify"` fails today**. Options:
  - **D1a (recommended for cutover): extend `rust-verify` additively** (§5 P1): add
    optional `args-json` + `contract-version` params with empty defaults and fold
    `args-json` into the `cargo test` argv. Zero impact on existing bash/NEEDLE
    callers (defaults preserve today's contract), rollback is a manifest revert via
    declarative-config, and "reproduces current behavior" stays literal.
  - **D1b (follow-up bead, not cutover-blocking): deploy contrib `gantry-verify`**
    (already in `contrib/argo/`, not yet applied to iad-ci) and pin gantry to it.
    Gains the full remote contract: verdict.json (OOM/deadline → infra_failure →
    local fallback instead of today's misread), failure-class taxonomy, contract
    handshake. Also fixes D1's residual below.
  - Residual under D1a (parity with the bash pair, warts included): a pod OOM-kill
    or the 1800s step deadline surfaces as workflow `Failed` → gantry classifies
    `test_failure` → no local retry. Mitigate cheaply by setting
    `[remote.argo] deadline_minutes = 25` (Appendix A) so the *client* gives up
    first and falls back locally. The structural fix is D1b.
- **D2 — first release does not exist.** Forgejo has **zero** releases and zero tags
  for `jedarden/gantry-rs`; `install.sh`'s default fetch (latest release) cannot
  work yet. Sub-options: (a) cut the first release via the live `gantry-ci`
  template (publishes to Forgejo, forge-ci pattern; note Forgejo anonymous access is
  login-walled, so even then these boxes install with an authenticated fetch), or
  (b) **recommended for the two known boxes**: stage a locally/template-built musl
  tarball and install with `GANTRY_INSTALL_LOCAL_BIN=…` (install.sh supports this
  exactly for air-gapped/staged installs). The curl-pipe fresh-box acceptance line
  stays a separate follow-up (P0-b) either way.
- **D3 — `/build/<repo>` CARGO_TARGET_DIR allocation (codinghome-critical).** Gantry
  has no equivalent; after cutover on codinghome, builds default to per-checkout
  `target/` dirs — the exact pattern that produced 190 dirs / 830G and a 100%-full
  disk (needle-d6b685b4), and the wrapper's *refusal* of off-`/build` target dirs
  also disappears. Options: (a) export `CARGO_TARGET_DIR` per repo via the fleet
  env the workers already inherit (`fleet-policy.env`/`environment.d`) — coarse but
  zero-code; (b) accept with disk monitoring (`df` alert at the 20G threshold this
  workspace already uses); (c) file the feature bead ("gantry target-dir policy")
  and hold codinghome until it lands. **Recommendation: (a) now, (c) behind it.**
  Lab is unaffected (no `/build` today — but confirm at sign-off; if `/build` was
  since created on lab, lab inherits this gap too).
- **D4 — slice placement.** The bash pair scopes runs into the **caller's** slice
  (needle.slice for workers — the fleet-containment fix). Gantry places every local
  run in **`gantry.slice`** (boxwide sum cap 12 CPU / 32G, per-run caps still 200%/6G).
  A NEEDLE worker's gantry-spawned build therefore stops counting against
  needle.slice and starts counting against gantry.slice — whose sum cap is roughly
  the whole lab box. Options: (a) accept (the sum cap still bounds the box;
  needle.slice ceilings were per-worker fairness, not the box guard); (b) set
  `slice_enabled = false` on worker boxes and accept per-run-caps-only; (c) feature
  bead: caller-slice placement mode. **Recommendation: (a) for the soak, (c) if the
  audit shows needle.slice fairness actually mattered.**
- **D5 — no `RuntimeMaxSec` on gantry scopes.** The 4h reap (needle-3d5c65d8) that
  prevents a SIGKILLed client from leaving a hung scope running for days has no
  gantry equivalent (`local.rs` scopes set CPUQuota/MemoryMax/MemorySwapMax only).
  A killed gantry client can leave an orphaned scope until its child exits naturally.
  Options: (a) accept for the soak with a daily `systemctl --user list-units
  'run-*.scope'` eyeball; (b) feature bead (per-run runtime cap config) before
  cutover. **Recommendation: (a), with (b) filed as follow-up — the fallback
  semaphore and slice cap shrink the blast radius vs. the 2026-09-03 incident.**
- **D6 — mechanics:** final box list (§2.3); whether cutover is a new
  `apply-lab-fleet.sh` verb or the manual §6 steps; cutover window (recommend:
  start of a NEEDLE quiet window, never mid-cycle; box order lab → codinghome);
  `.bak` deletion date (cutover + 7d, §9).

## 5. Pre-flight (cluster/repo side — before any box is touched)

- **P0 — release artifact (D2).** Stage the musl release tarball to a path both
  boxes can read (or build via `gantry-ci`). Record the exact version string in the
  cutover log. Parallell follow-up (non-blocking): cut the first Forgejo release and
  fix the anonymous-fetch caveat for curl-pipe installs.
- **P1 — `rust-verify` additive extension (D1a).** Edit
  `declarative-config/k8s/iad-ci/argo-workflows/rust-verify-workflowtemplate.yml` —
  **via declarative-config commit + push + ArgoCD sync, never kubectl** (org rule) —
  adding two parameters with empty-string defaults:
  ```yaml
  - name: args-json         # gantry faithful-argv contract: JSON array of argv after the subcommand
    value: ""               # empty = not a gantry run; legacy test-args callers unaffected
  - name: contract-version  # gantry remote-contract handshake; unused by rust-verify today
    value: ""
  ```
  and, in the `verify` step script, resolving test args safely — **array, never
  word-split** (that would reintroduce the bug INV-5 fixes):
  ```bash
  if [[ -n "${ARGS_JSON:-}" && "${ARGS_JSON}" != "[]" ]] && [[ -n "${TEST_ARGS:-}" ]]; then
    echo "FAILED: both test-args and args-json supplied — ambiguous" >> /tmp/verify-output; exit 1
  fi
  mapfile -t EXTRA < <(jq -r '.[]' <<<"${ARGS_JSON:-[]}")
  # ... existing cargo test line becomes:  cargo test "${EXTRA[@]}"
  ```
  Verify post-sync: submit a manual Workflow with `args-json='["-p","gantry"]'` style
  args through the read-only path (`kubectl create` of a Workflow in argo-workflows
  is permitted) and confirm the args appear verbatim in the pod's cargo invocation.
  The bash pair's `test-args` path must still work (regression guard for the fleet).
- **P2 — baseline captures (feeds §7 audits).** From each box, for every
  fleet-active repo (enumerate `~/*/` with an origin), snapshot:
  `git ls-remote <url> 'refs/heads/*' | sort > ~/gantry-migration/<box>-heads-before.txt`
  and record `sha256sum ~/.local/bin/cargo ~/.local/bin/cargo-remote`.
- **P3 — pause the drift watcher** (`wrapper-drift.timer`) so cutover doesn't trip
  it; re-enable decision lands in §9.

## 6. Cutover (per box; lab first = canary, codinghome second)

Execute only after §10 sign-off. All commands as the `coding` user. If any
verification step fails, **stop, roll back (§8), and record the failure on the
bead** — do not proceed to the second box with a red canary.

1. **Backup the pair (cheap, do it even though `--force` also backs up):**
   ```bash
   mkdir -p ~/gantry-migration
   cp -a ~/.local/bin/cargo-remote ~/.local/bin/cargo-remote.pre-gantry.bak
   sha256sum ~/.local/bin/cargo ~/.local/bin/cargo-remote | tee ~/gantry-migration/bash-pair.sha256
   ```
2. **Pause `wrapper-drift.timer`** (P3) — confirm with `systemctl list-timers` (user
   or system scope, wherever the operator parked it).
3. **Install gantry over the foreign shim:**
   ```bash
   curl -fsSL https://git.ardenone.com/jedarden/gantry-rs/raw/branch/main/install.sh -o /var/tmp/gantry-install.sh
   GANTRY_INSTALL_LOCAL_BIN=<staged>/gantry-<ver>-x86_64-linux.tar.gz \
     sh /var/tmp/gantry-install.sh install --force
   ```
   `--force` moves the foreign `cargo` to `cargo.pre-gantry.bak` (kept, not deleted)
   and refuses anything it cannot prove is safe to replace. `install.sh` writes no
   config (Tier-0 by construction) — the next step supplies it. Note: authenticated
   fetch for the script itself may be needed (Forgejo login wall); staging the
   script beside the tarball sidesteps it.
4. **Write the config** (Appendix A verbatim, modulo the D1/D3/D4 decisions) to
   `~/.config/gantry/config.toml`. Gantry has no per-key config-origin display;
   confirm the file took effect with the surfaces it does have:
   ```bash
   gantry explain -- cargo test   # in a clean clone: backend `argo`, push ref — the user config in effect
   cargo --version                # any shim invocation: zero `[gantry] config warning:` lines on stderr
   ```
   Config problems (`unknown key`, trust-boundary ignores) print as
   `[gantry] config warning: …` at every gantry start — silence means the file
   parsed clean. Layer provenance is structural, not displayed: the repo layer
   cannot set `ci_remote`/`push_mode` (trust boundary S-2 — a stray repo-level key
   is ignored with a warning), so the backend and push mode `explain` reports can
   only come from the user config.
5. **Static checks, in order:**
   ```bash
   which cargo                 # → ~/.local/bin/cargo (the shim)
   gantry quickcheck           # exit 0 — shim resolves, cap works, git ok
   gantry doctor               # exit 0 — incl. argo preflight: kubectl + kubeconfig + template
   ```
6. **Decision checks (no network):** in a scratch clone of gantry-rs (clean tree),
   `gantry explain -- cargo test` → plan shows: gates green, backend argo, ref
   `refs/gantry/<epoch>-<sha>`; then `touch UNTRACKED && gantry explain -- cargo
   test` → plan flips to local, reason `dirty`; `rm UNTRACKED`. Restore a clean tree.
7. **Canary round trip:** `gantry doctor --e2e` → exit 0. This pushes a real epoch
   ref and round-trips the configured backend — the single best "is the pipeline
   actually working" signal, and it must be green before any real traffic.
8. **Real-run smoke:** in the scratch clone, `cargo test --lib` (or the repo's
   cheapest suite). Expect: `[gantry]` decision line, workflow `gantry-…` on iad-ci,
   streamed logs, verdict trailer, exit 0; `gantry status` and
   `jq -c . ~/.local/state/gantry/runs.jsonl | tail -2` show an intent+verdict pair
   with `"ran":"remote"`. Then break something on purpose (scratch commit with a
   failing test, push it as a scratch branch) and confirm verdict `test_failure`,
   exit non-zero, **and** `git ls-remote origin refs/heads/*` unchanged (first live
   INV-2 datapoint).
9. **Fleet transparency check:** from a shell shaped like a worker's
   (`GANTRY_AGENT=1`), run the passthrough `cargo --version` and confirm
   sub-5ms-feel passthrough and no stray output; run one capped local path (e.g.
   `cargo test` in a dirty scratch tree) and confirm the `[gantry]` lines + scope in
   `gantry.slice` (`systemd-run` unit visible via `systemctl --user`).
10. **Record the cutover** on the migration bead: install version, config decisions
    taken, e2e/smoke outputs, timestamp. This box's soak clock starts now.

**codinghome** repeats steps 1–10 after lab has soaked ≥24h clean, plus:
- the D3 target-dir decision applied **before** step 8 (else first builds cold-start
  into `~/…/target/`);
- confirm `/build` exists (D3 applies) and `df -BG /` headroom ≥ 20G.

**Rolling workers:** in-flight NEEDLE workers are unaffected (already-exec'd bash);
fresh shells pick up the gantry shim on their next `cargo` invocation. No worker
config change is required (PATH inheritance). Optional: export `GANTRY_AGENT=1` via
fleet policy env so workers queue behind humans in fallback storms.

## 7. Soak-week verification protocol (one full fleet cycle)

The ledger is the record of truth — iad-ci workflows TTL-reap (30 min success / 2 h
failure) and the Argo UI is *not* an acceptable audit source.

**Daily (both boxes, ~5 min):**
```bash
gantry doctor                                   # exit 0; zero orphaned-intent lines
gantry status --json | jq '.in_flight | length' # bounded, drains to 0
df -BG / | tail -1                              # ≥ 20G free (early-warning, D3)
git ls-remote origin 'refs/gantry/*' | wc -l    # epoch refs sweep toward 0 (GC steady state)
```
Plus eyeball `systemctl --user list-units 'run-*.scope'` for hung scopes (D5) and
box load during fallback storms (AS-3: serialized trickle, never a stampede).

**INV-1 audit — no silent skips (end of cycle):**
```bash
R=~/.local/state/gantry/runs.jsonl
jq -r 'select(.rec=="intent")  | .run_id' $R | sort > /tmp/i.txt
jq -r 'select(.rec=="verdict") | .run_id' $R | sort > /tmp/v.txt
comm -23 /tmp/i.txt /tmp/v.txt        # intents with no verdict = orphans
gantry doctor                         # must report the same set, named as lost runs
```
Pass criteria: (a) `comm` output matches exactly what `gantry doctor` flags — zero
*undetectable* gaps (INV-1 permits detectable orphans, e.g. a documented SIGKILL);
(b) transcript cross-check: sample ≥10 worker transcripts from the cycle, count
`cargo test` invocations per box-window, and reconcile against intents in the same
window — every intercepted invocation has an intent record. Known-benign
mismatches to exclude: `cargo build`/`cargo check` passthrough (not intercepted),
invocations from before the box's cutover timestamp.

**INV-2 audit — no branch pushes (end of cycle):**
```bash
git ls-remote <url> 'refs/heads/*' | sort > <box>-heads-after.txt   # per repo (P2 baseline)
diff <box>-heads-before.txt <box>-heads-after.txt
```
Pass criteria: (a) every changed/added head maps to an explicit human/worker push
(Forgejo activity, `git log` on the branch) whose timestamp falls **outside** every
gantry remote-run window for that repo in `runs.jsonl` — i.e. zero movements
attributable to gantry; (b) `grep push_mode ~/.config/gantry/config.toml` on both
boxes still reads `push_mode = "ref"` and shim runs print no
`[gantry] config warning:` lines (config-drift guard; the repo layer cannot
override `push_mode` — trust boundary S-2); (c) `refs/gantry/*`
trend to zero at steady state (per-box GC sweeps only its own refs, EC-09).

**Watch-items during the week:**
- **W1 — fallback ratio.** A sudden rise in `ran: local_after_infra` means iad-ci or
  the P1 template regressed; `gantry report <run-id>` has the bundle.
- **W2 — orphaned dependency on cargo-remote's branch push.** Anything that
  implicitly relied on `cargo test` publishing the caller's branch to origin
  (dispatch orchestrators checking `ls-remote` for prior work, predictors, humans
  glancing at origin) will now see stale branches until an explicit push. The NEEDLE
  pre-close checklist pushes explicitly, so gates should be unaffected — but any
  "unknown revision" / "prior work not found" pattern during soak points here.
- **W3 — dirty-rate shift (§4.2 item 2).** Runs newly local with reason `dirty` are
  the untracked-file gate working; confirm a couple by hand.
- **W4 — D4 slice accounting.** Spot-check that worker-spawned builds appear under
  `gantry.slice` and box load stays inside 12 CPU / 32G with several concurrent runs.

## 8. Rollback (fastest first; each tier subsumes the previous)

1. **Kill switch (seconds, reversible):** `gantry off` → every run local capped
   (state file, no file swaps). Diagnose at leisure; `gantry on` to resume.
2. **Restore the pair (minutes):**
   ```bash
   mv ~/.local/bin/cargo.pre-gantry.bak ~/.local/bin/cargo
   mv ~/.local/bin/cargo-remote.pre-gantry.bak ~/.local/bin/cargo-remote
   ```
   (On codinghome also undo the D3 env change.) Config/state may stay; nothing
   reads it once the shim is gone. Re-enable `wrapper-drift.timer`.
3. **Full uninstall:** `gantry uninstall` (removes only provably-gantry artifacts —
   binary, shims, state, config (`--keep-config` to keep), slice unit — and reports
   anything it will not touch). P1's `rust-verify` extension reverts via
   declarative-config revert + ArgoCD sync if desired; the empty-default params are
   inert for legacy callers and need no urgency.

## 9. Post-soak (cutover + 7 days)

- Audits (§7) green + no open watch-items → delete the `.bak` pair on both boxes;
  re-enable or retire `wrapper-drift.timer` (retire: the thing it watched is gone;
  retire it in the same dotfiles commit that removes the wrapper from
  `apply-lab-fleet.sh --install-cargo-wrapper`).
- Remove `cargo-remote` from the dotfiles deploy surface; keep the tracked copies in
  git history (they are the provenance for the parity table in this runbook).
- File/confirm follow-up beads: D1b (`gantry-verify` cutover), D3 (target-dir
  policy), D4 (caller-slice mode, if W4 warrants), D5 (per-run runtime cap), P0-b
  (Forgejo release + curl-pipe acceptance), plan.md Phase 4 entry check (fleet is on
  gantry).

## 10. Sign-off block — explicit human approval required

Nothing below is executed until this block is completed. Approve per item; "no" on
any D-item blocks cutover until resolved.

```text
Phase 3 migration sign-off — prepared by bead bf-3c8 (runbook:
docs/notes/phase3-migration-runbook.md; the delivery commit sha is recorded
on the bead at close)

[ ] Scope approved (§1): boxes = lab, codinghome (D6 box list confirmed; ex44 N/A)
[ ] D1 template contract: D1a (extend rust-verify, §5 P1) — approved / rejected
[ ] D2 install path: staged GANTRY_INSTALL_LOCAL_BIN (§5 P0) — approved / rejected
[ ] D3 codinghome CARGO_TARGET_DIR: option (a)/(b)/(c) — chosen: ______
[ ] D4 slice placement: option (a)/(b)/(c) — chosen: ______
[ ] D5 RuntimeMaxSec gap: option (a) with follow-up bead / (b) pre-cutover — chosen: ______
[ ] D6 mechanics: cutover window ______ ; box order lab→codinghome ; .bak deletion
    date (cutover+7d) ______ ; executor (operator / named agent): ______
[ ] Soak protocol + audit pass criteria (§7) accepted
[ ] Rollback authority acknowledged (§8)

Signed: ______________  date: ________
```

---

## Appendix A — deployed user config (`~/.config/gantry/config.toml`)

Reproduces current behavior per the parity table; D-decision values marked inline.

```toml
# Phase 3 migration config — lab + codinghome (bf-3c8 runbook, Appendix A).
# Reproduces the bash pair: argo backend -> iad-ci rust-verify, same caps.

[local]
cpu_quota_pct = 200              # = bash pair CPUQuota=200%
memory_max    = "6G"             # = bash pair MemoryMax=6G
cap_passthrough = true           # = bash pair caps every invocation
# fallback_slots / fallback_wait_secs: defaults (3 / 3600) — no bash counterpart
# slice_enabled: default true (gantry.slice 12 CPU / 32G) — see runbook D4
# slice_cpu_quota_pct / slice_memory_max: defaults (1200 / "32G")

[tool.cargo]
intercept   = ["test"]           # = bash pair intercepts only `test`
# real_binary: unset — PATH resolution with shim-dir stripping finds ~/.cargo/bin/cargo

[remote]
backend    = "argo"
ci_remote  = "origin"
push_mode  = "ref"               # epoch refs only; INV-2 — never a branch push
deadline_minutes = 25            # D1a: under rust-verify's 1800s step deadline so a
                                 # stalled run degrades to capped-local (infra), not a
                                 # misread test failure. Raise to 40 if D1b lands.

[remote.argo]
kubectl_path = "kubectl"
kubeconfig   = "~/.kube/iad-ci.kubeconfig"   # = bash pair KUBECONFIG_PATH default
namespace    = "argo-workflows"
template     = "rust-verify"     # requires pre-flight P1 (args-json param) — D1a
generate_name = "gantry-"
base_url     = "https://argo-ci.ardenone.com"
# builder_image: unset — template default (ronaldraygun/needle-ci-builder pin) applies
```

## Appendix B — audit command reference

Quick index of §7's commands (all safe/read-only; `kubectl` uses the read-only proxy
or the same kubeconfig the backend uses):

```bash
# ledger sanity (per box)
jq -c 'select(.rec=="intent")'  ~/.local/state/gantry/runs.jsonl | tail -5
jq -r 'select(.rec=="intent")  | .run_id' ~/.local/state/gantry/runs.jsonl | sort > /tmp/i
jq -r 'select(.rec=="verdict") | .run_id' ~/.local/state/gantry/runs.jsonl | sort > /tmp/v
comm -23 /tmp/i /tmp/v                       # orphans (INV-1); must equal `gantry doctor` output

# decision replay for any run
gantry why --json | jq .                     # last run's gate/decision trace
gantry status --json | jq '.recent[:5]'      # recent runs, verdicts, ran-location

# remote footprint (per repo)
git ls-remote origin 'refs/gantry/*'         # epoch refs; sweeps to 0 at steady state
git ls-remote origin 'refs/heads/*' | sort   # diff vs §5 P2 baseline (INV-2)

# iad-ci side, when a run needs corroboration
kubectl --server=http://traefik-iad-ci:8001 get workflows -n argo-workflows \
  --sort-by=.metadata.creationTimestamp | grep gantry- | tail
```
