# Threat model (S-6)

**Status:** published companion to the public README (plan §"Security
considerations", S-6). Every claim below names the code or test that enforces
it — the enforcing tests are part of the definition of done (plan §"Quality
gates"), not prose supporting material.

Gantry is a PATH shim that intercepts `cargo`, ships the already-committed
HEAD to a remote executor, and returns the real verdict. This document states
what a running gantry can and cannot touch: the refs it creates on your git
remote, how much of your repository's own configuration it obeys, where
credentials come from, and where it may open network connections.

## 1. Trust model and scope

Three parties with three different trust levels:

1. **The caller** (human or agent) runs a vanilla `cargo` command and delegates
   to gantry: read-only inspection of git metadata, epoch-ref pushes to the
   configured `ci_remote`, and dispatch of the committed revision to the
   configured backend. The interception surface is argv[0] only — dispatch
   keys on the invocation name, nothing deeper (src/shim.rs:27-48,
   `invocation_name`).
2. **The repository under test** is untrusted in two separate ways:
   - its **config** (`.gantry.toml`) is untrusted input to configuration
     loading — bounded in §3;
   - its **code** is executed *by design* (locally under a cgroup cap, or
     remotely via the backend). Gantry bounds its own authority; it does not
     sandbox the test suite it is asked to run — that is the product (§6).
3. **The backend** (Argo Workflows cluster, SSH host, or command template) is
   chosen exclusively by user/system config, never by the repo (§3). Whatever
   the backend may do, the user delegated to it explicitly.

## 2. What gantry can touch — complete inventory

**Filesystem reads:** the repo's git metadata via read-only porcelain —
`git status --porcelain` (src/gate.rs:248), the `rev-parse` family for
work-tree/common-dir/HEAD (src/gate.rs:73-213), `git remote get-url`
(src/gate.rs:236), `git ls-remote refs/gantry/*` (src/refs.rs:459); the three
config layers (system, user, repo — §3); environment variables; and its own
state dir `~/.local/state/gantry/` (src/state.rs:101).

**Filesystem writes:** only its own state dir — `state.toml` kill-switch file
(src/state.rs), `runs.jsonl` runlog (src/runlog.rs:29), `leases.jsonl` ref
leases (src/refs.rs:51), the last-known-good config snapshot, and the Tier-0
notice timestamp (src/decision.rs, `note_tier0`) — plus throwaway temp dirs
for the `doctor --e2e` canary world (src/doctor.rs:609-617). **It never writes
inside the repo:** no production code path writes to the work tree or `.git`
(the only `git config` invocations in the tree are test fixtures), and the
GitGate refuses to run at all unless the tree is clean — enforced by
`repo_with_uncommitted_change_fails_clean_tree_check` (src/gate.rs:627) and
`repo_with_untracked_file_fails_clean_tree_check` (src/gate.rs:691). Shipping
only committed revisions is the gate's first property: the run pushes the
exact HEAD sha (src/refs.rs:558) that `git status --porcelain` vouched clean.

**Git mutations:** exactly two shapes, both to the configured `ci_remote`
only — create `refs/gantry/<epoch>-<sha>` via
`git push <ci_remote> <sha>:refs/gantry/<epoch>-<sha>` (ref name built at
src/refs.rs:261, executed at src/refs.rs:558), and GC-delete an expired
own-ref via `git push <ci_remote> :<ref>` (src/refs.rs:511). No fetch, pull,
merge, rebase, clone, checkout, or config write exists in production code
(the only production `git config` invocations are doctor's reads of
`user.name`/`user.email`, src/doctor.rs:408-409; every other one in the tree
is a test fixture).

**Processes spawned:** git (above); `kubectl` for the argo backend
(src/backend/argo.rs:362-369); the user-configured command-template argv
(src/backend/command.rs); `systemd-run --scope` for local cgroup caps
(src/cap.rs, launch line at src/cap.rs:9-12); the real toolchain for local
runs.

**Network:** the git transport to the `ci_remote` and the backend's endpoint —
and nothing else, asserted rather than promised (§5).

## 3. Untrusted repo config (S-2)

`.gantry.toml` is the repo layer of the three-layer config
(src/config.rs:1-5,168). It cannot alter what makes remote execution *reach*
anything:

| Repo layer may not set | Behavior | Enforcing test (src/config.rs) |
|---|---|---|
| `remote.ci_remote` (push target) | ignored with a loud warning | `trust_boundary_blocks_repo_ci_remote` :1799 |
| `remote.push_mode` (ref vs branch) | ignored with a loud warning | `trust_boundary_blocks_repo_push_mode` :1818 |
| the whole `[remote.command]` table (executable argv templates) | layer rejected, fail-closed to last-known-good | `trust_boundary_blocks_repo_command_backend` :1837 |
| fallback semaphore caps (`fallback_slots`, `fallback_wait_secs`) | ignored with warning | `repo_layer_cannot_touch_fallback_semaphore_keys` :2162 |
| slice caps (`slice_enabled`, `slice_cpu_quota_pct`, `slice_memory_max`) | ignored with warning | `repo_layer_cannot_touch_slice_cap_keys` :2331 |

The reverse direction holds: trusted layers can set all of it
(`trust_boundary_trusted_layers_can_set_ci_remote_and_push_mode` :2748,
`trust_boundary_allows_command_from_user_config` :1858), so the boundary is a
real restriction, not a dead check. The one thing the repo layer *can* do is
narrow: select no backend (opt-out) — `trust_boundary_repo_can_narrow_backend_to_none`
:2773. It cannot enable a backend, change a push target, or supply executable
command templates. A repo layer that fails to parse is rejected whole,
fail-closed to the last-known-good snapshot (src/config.rs:721-730).

Two adjacent honesty notes:

- **The LKG snapshot skips the trust boundary when parsed**
  (src/config.rs:1086-1087) — deliberately: the snapshot is written by gantry
  itself out of already-merged values and lives in the user's state dir, not
  in the repo, so repo-supplied bytes never reach that parser.
- **Command templates are argv arrays** with placeholder substitution at the
  argv level — no shell interpolation anywhere (src/backend/command.rs:14-24,37-39;
  plan S-4/EC-06). A corrupt-config run serves the snapshot with a banner
  instead of crashing (`lkg_corrupted_config_serves_snapshot_with_banner` :2989).

## 4. Credential posture (S-3, S-5)

Gantry stores no credentials and handles no token values:

- The argo backend's config carries a kubeconfig **path**, not content
  (src/backend/argo.rs:264-265), handed to `kubectl` as a flag
  (src/backend/argo.rs:367-369,671-673); empty means the cluster default.
- `git push` / `git ls-remote` authenticate through ambient git (credential
  helpers, SSH agent). Gantry never reads, writes, or transforms those
  credentials — it constructs argv and spawns git.
- Log hygiene (S-5): the runlog stores the repo remote URL with the userinfo
  component stripped before it is written (src/runlog.rs:409-412, applying
  `crate::crash::redact_url_userinfo`) — the same single implementation the
  crash bundle uses, so the rule has one audit point. Enforced by
  `intent_repo_strips_embedded_credentials_s5` (src/runlog.rs:853).
- Crash bundles are redacted before hitting disk: URL userinfo
  (src/crash.rs:598), scheme-embedded credentials (:629), key/value pairs
  whose key names look secret-bearing (:711), known token shapes even without
  a key (:824), private key blocks whole (test
  `private_key_blocks_are_redacted_whole` src/crash.rs:1287), the manifest
  included (`the_manifest_is_redacted_like_every_other_artifact` :1273), and
  end-to-end via `record_in_writes_a_redacted_bundle` :1428. Redaction is
  idempotent (`a_second_redaction_pass_is_byte_identical_to_the_first` :1172)
  and applied before the size cap (`redaction_is_applied_before_the_size_cap`
  :1312).

**One deliberate disclosure:** the local runlog (`~/.local/state/gantry/runs.jsonl`)
records the caller's argv as typed (`IntentRecord.args`, src/runlog.rs:362),
under the user's own HOME with the state dir's usual file permissions. A
secret passed as a CLI argument would land there; gantry redacts URLs, not
argv, and never inspects argv beyond subcommand interception.

## 5. Network confinement (INV-6, S-7)

The pledge — no connections except the configured backend and the git remote —
is CI-enforced, not prose (plan S-7). `tests/no_phone_home_integration.rs`
drives the real binary through the production pipeline (config load → GitGate →
epoch-ref push → backend dispatch → executor → verdict) with an LD_PRELOAD
observer (tests/no_phone_home/netobserver.c, built at test time) recording
every AF_INET/AF_INET6 `connect`/`sendto`/`sendmsg` in the run's process tree —
gantry, git, the executor shell, the toolchain — into a netlog.

The loopback world (bare `file://` remote, reference executor behind the
command-template backend) has **no legitimate IP destination**, so the
assertion is the empty allow-list: any record is a phone-home, whatever its
destination (`a_full_remote_run_opens_no_ip_connection`, :477). Two guards
keep it non-vacuous:

- **Positive control** `the_observer_records_attempts_and_is_inert_without_the_preload`
  (:197) — a silently broken preload would otherwise let the main assertion
  pass for free.
- **Mutation run** `a_connecting_fixture_is_caught_as_phone_home` (:507) — a
  fixture that dials a loopback listener from inside the run's own process
  tree trips the assertion with `phone-home detected` naming the peer.

The observer documents its own reach honestly (test header :27-32): static
binaries, raw syscalls, and glibc-internal resolver traffic can bypass
LD_PRELOAD interposition. What the test enforces is the realistic regression
surface — no code linked into the run may reach the network through the normal
libc socket API, which is exactly how a telemetry or update-check dependency
would phone home. From Phase 3 on this suite is a release gate (plan §"Quality
gates" item 5).

## 6. Ref-leak surface

The predecessor tool ran an implicit branch push, which on 2026-07-19 pushed
an unfinished local `main` to a public mirror-fed remote — the incident that
made "no implicit branch pushes" a design decision (docs/notes/design-decisions.md,
DD-2; plan S-1). Gantry's replacement contract:

- **Only hidden refs are created.** Default `push_mode = ref` writes a fresh
  `refs/gantry/<unix-epoch>-<sha>` per run and never a branch. Enforced by
  `push_does_not_modify_branch_refs` (src/refs.rs:779) and end-to-end by
  `test_no_branch_refs_move_during_round_trip` /
  `test_only_gantry_refs_are_created_during_round_trip`
  (tests/integration.rs:314,322), which diff complete `ls-remote` output
  before/after a run and require only `refs/gantry/*` to appear (INV-2).
  A Tier-0 local run touches the remote not at all —
  `no_config_intercepted_run_executes_locally_with_full_runlog_treatment`
  (tests/tier0_integration.rs:364, remote-untouched assertion at :426).
- **In-code tripwire:** `RefPusher::assert_no_branch_refs_moved`
  (src/refs.rs:578) re-compares `refs/heads/*` across a push.
- **Legacy branch mode is loud and trusted-only.** `push_mode = branch`
  creates a mutable `refs/heads/gantry/<sha>` and warns on every push
  (src/refs.rs:206-210) — and the repo layer cannot select it (§3,
  `trust_boundary_blocks_repo_push_mode`).
- **GC touches only what this box created.** Every successful push writes a
  local lease record (src/refs.rs:306; `push_creates_lease_record` :820). The
  opportunistic sweep after each push (src/refs.rs:231, best-effort — sweep
  failure is a warning, never a run failure) deletes a remote ref only when a
  *local* lease matches it and the ref's epoch is older than the 7-day
  retention (`DEFAULT_RETENTION_DAYS` src/refs.rs:25, lease window
  src/refs.rs:22). A box never deletes another box's refs — cross-box lease
  files do not exist by design (plan EC-09).
- **Residual — mirror propagation (R8).** Server-side push mirrors (e.g.
  Forgejo→GitHub) sync all refs, so `refs/gantry/*` can propagate to a public
  mirror. The proposed mirror-leak guard (detect a mirror-fed `ci_remote`,
  warn or refuse) was skipped by the owner and remains on the bench
  (docs/notes/ideas-ledger.md, finalist 3). Interim mitigation: point
  `ci_remote` at a dedicated, non-mirrored remote on mirrored repositories.
  The speculative pre-push-hook idea was killed in part because it would have
  *enlarged* this surface (docs/notes/ideas-ledger.md, "Speculative pre-push").

## 7. What gantry does not defend against

- **The repo's test code is arbitrary code.** It runs locally (capped through
  a systemd scope with CPUQuota/MemoryMax and swap off, src/cap.rs:9-12;
  `tier0_run_is_capped_through_a_systemd_scope_when_available` /
  `tier0_run_degrades_loudly_when_no_scope_is_available`,
  tests/tier0_integration.rs:586,605) or remotely via the backend. Gantry
  bounds its own authority; sandboxing untrusted test suites is a non-goal.
- **Backend-side execution** of the pushed sha happens under the user's
  configured template, inside the trust domain the user chose — outside
  gantry's process and outside this model's reach.
- **Mirror propagation of `refs/gantry/*`** (R8, §6) — benched, not enforced.
- **LD_PRELOAD interposition gaps** (§5) — documented by the test itself.
- **Local argv disclosure in the runlog** (§4) — by design, user-scoped.

## 8. Invariant → enforcement map

| INV | Pledge | Enforcement |
|---|---|---|
| INV-1 | No silent skip | Write-ahead runlog + orphan detection: src/runlog.rs (intent records), `check_orphaned_intents` src/doctor.rs:441 |
| INV-2 | No branch mutation | tests/integration.rs:314,322; src/refs.rs:779; `assert_no_branch_refs_moved` src/refs.rs:578 |
| INV-3 | Exit-code fidelity | Verdicts derive from status APIs, never log parsing: src/verdict.rs, argo name-token parse src/backend/argo.rs:801,1533 |
| INV-4 | <5ms passthrough budget | benches/ hyperfine gate (plan §"Quality gates" item 3) |
| INV-5 | Argv round-trip byte-exact | `round_trip_preserves_semantics` src/backend/mod.rs:420; argv arrays end-to-end, src/backend/command.rs |
| INV-6 | Network confinement | §5 — tests/no_phone_home_integration.rs |
| INV-7 | No local retry of real failures | Deadline ladder: only infra failures downgrade to a capped local rerun (`ran: local_after_infra`); earned verdicts are returned as given — tests/argo_deadline_expiry_integration.rs:302-377 |
