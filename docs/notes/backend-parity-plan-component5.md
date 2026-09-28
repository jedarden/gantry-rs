# Backend parity audit — plan Component 5 vs `src/backend/mod.rs`

*Audit date: 2026-09-27 (bead gantry-cf184b49, child of bf-48oi). Compared
`src/backend/mod.rs` against `docs/plan/plan.md` §Component 5 ("RemoteBackend
trait + implementations"), §"Data models" (RunSpec), and §"Verdict semantics".
The trait landed in `64e81f4` (bf-2iwc), the full verdict ladder in `b9e7464`
(bf-67m6), placeholder substitution in `eafe438` (bf-r97u).*

**Result: full parity after one fix.** Every plan-mandated trait method,
type, and verdict variant is present with the plan signature; the single
divergence found (`RunSpec.cwd_rel` typed `String` instead of the plan's
`PathBuf`) was fixed in the same commit as this note. No follow-up bead was
needed.

## 1. Trait methods — plan block vs shipped signatures

| Plan §5 signature | Shipped (`src/backend/mod.rs`) | Parity |
|---|---|---|
| `fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError>` | `fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError>` (required, mod.rs:200) | ✅ exact |
| `fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError>` (best-effort) | `fn stream_logs(&self, h: &RunHandle, out: &mut dyn std::io::Write) -> Result<(), BackendError>` (mod.rs:206; `std::io::Write` is the plan's unqualified `Write`) | ✅ exact |
| `fn wait(&self, h: &RunHandle, deadline: Instant) -> Result<Verdict, BackendError>` (authoritative) | `fn wait(&self, h: &RunHandle, deadline: std::time::Instant) -> Result<Verdict, BackendError>` (required, mod.rs:218) | ✅ exact |
| `fn describe(&self, h: &RunHandle) -> String` | `fn describe(&self, h: &RunHandle) -> String` (mod.rs:225) | ✅ exact |
| `fn cancel(&self, h: &RunHandle) -> Result<(), BackendError>` (Ctrl-C propagation) | `fn cancel(&self, h: &RunHandle) -> Result<(), BackendError>` (mod.rs:235) | ✅ exact |

Notes:

- `stream_logs`, `describe`, and `cancel` ship with default bodies that panic
  ("not implemented in Phase 0.5") so a minimal implementor only needs
  `submit` + `wait`. Both real backends override all three
  (`CommandBackend`: src/backend/command.rs:236/273/281; `ArgoBackend`:
  src/backend/argo.rs:602/708/724). The plan does not forbid defaults; the
  panic convention is pinned by test `default_status_follows_phase05_panic_convention`.
- `cancel` on `CommandBackend` deliberately returns a documenting
  `Err("cancel is not supported for command backend")` — the command contract
  (plan §5, "`command` (v1)") defines only `submit`/`logs`/`wait` argv, so
  there is no cancel template to run yet. Signature parity holds; the Err is
  the honest implementation of a contract gap, not trait drift.
- **Additive beyond plan (sanctioned):** `fn status(&self, h: &RunHandle) ->
  Result<RunStatus, BackendError>` (mod.rs:250), added in gantry-5601513f as
  a best-effort, never-blocking polling snapshot. It returns no verdict —
  `wait()` stays the only verdict authority — and is recorded as canonical
  vocabulary in `docs/notes/backend-type-naming.md`. Superset, not drift.

## 2. Types — plan vs shipped

| Plan type | Shipped | Parity |
|---|---|---|
| `RunSpec { tool: String, subcommand: String, args: Vec<String>, repo_url: String, sha: String, cwd_rel: PathBuf }` | Same six fields, same names (mod.rs). **`cwd_rel` was `String` — fixed to `PathBuf` in this commit.** `RunSpec::new` still takes `cwd_rel: &str` ("" = repo root) and converts, so call sites are unchanged. | ✅ after fix |
| `RunHandle` | `{ handle: String }` + `RunHandle::new(&str)` (mod.rs). Plan names the type but leaves its shape unspecified. | ✅ compatible |
| `BackendError` | `{ reason: String }` + `Display` + `std::error::Error` (mod.rs). Shape unspecified in plan. | ✅ compatible |
| `Verdict` (§"Verdict semantics") | Full ladder, below. | ✅ exact |

`cwd_rel` rationale: plan §"Data models" types it `PathBuf` and the sibling
write-ahead ledger (`IntentRecord.cwd_rel`, src/runlog.rs:277) already ships
`PathBuf`; `RunSpec` was the last `String` holdout. No code outside mod.rs
reads the field directly (the only reader was mod.rs's own unit test), so the
type change is behavior-neutral today and puts remote `cd`-into-`cwd_rel`
handling (plan Component 5, RunSpec paragraph) on the right footing.

## 3. Verdict ladder — §"Verdict semantics" vs shipped enum

Plan requires six terminal values; the runs.jsonl terminal record names them
`pass | test_failure | gate_failure | infra_failure | superseded | cancelled`.

| Plan value | Shipped variant | serde (snake_case) | Exit code (to_exit_code) | Parity |
|---|---|---|---|---|
| `Pass` — exit 0 | `Verdict::Pass` | `pass` | 0 | ✅ |
| `TestFailure` — exit non-zero, **no local retry** (INV-7) | `Verdict::TestFailure` | `test_failure` | 1 | ✅ |
| `GateFailure` — exit non-zero, attributed to `[gantry] gate:`, no local retry | `Verdict::GateFailure` | `gate_failure` | 1 (non-zero ✅) | ✅ |
| `InfraFailure` — no verdict produced → capped local run, whose exit code wins | `Verdict::InfraFailure` | `infra_failure` | 2 (fallback signal; `from_exit_code(≥2) = InfraFailure` matches the plan's command contract "0 pass, 1 test-failure, ≥2 infra-failure") | ✅ |
| `cancelled` (user Ctrl-C → exit 130) | `Verdict::Cancelled` | `cancelled` | 130 | ✅ |
| `superseded` (v1.x) | `Verdict::Superseded` | `superseded` | 0 (plan leaves it unspecified) | ✅ |

Supporting semantics checked against the plan:

- OOMKilled / deadline-exceeded classify as **InfraFailure, never
  TestFailure** — `VerdictJson::to_verdict` puts `oom`/`deadline_exceeded`/
  workflow `Error` phase first, ahead of gate attribution and the exit-code
  ladder (plan §"Verdict semantics" ¶2, §Failure modes).
- `gate_failure` requires explicit attribution via
  `failure_class: "gate-failure"`; exit-code-only interpretation stays
  conservative (plan §AS-5, §argo remote contract).
- Absent/unparseable/unknown-schema verdict.json degrades to
  exit-code-only (`Verdict::interpret`), per plan §argo and §Versioning.
- The command contract ladder (`from_exit_code`) is total across `i32`
  including negative signal-derived codes — property-tested.

## 4. Beyond Component 5's letter, present and consistent with it

These are Phase 1a additions the plan calls for elsewhere and live in
mod.rs by design (plan §"Proposed module layout": "src/backend/mod.rs —
RemoteBackend trait + verdict.json parsing"):

- `VerdictJson` (schema_version 1: `schema_version`, `phase`, `exit_code`,
  `oom`, `deadline_exceeded`, optional `failure_class`) — plan §argo
  verdict.json paragraph; unknown fields ignored, unknown failure classes
  read as absent, unsupported schema versions are a loud error.
- `FailureClass` (`compile-error` / `test-failure` / `doctest` /
  `harness-panic` / `gate-failure`) — the plan's failure taxonomy, derived
  from cargo's `--message-format json` stream.
- `RunStatus` (`Pending`/`Running`/`Completed`/`Unknown`) — the coarse
  polling type behind the additive `status()`; per
  `docs/notes/backend-type-naming.md` it must never carry a verdict.

## 5. Drift filed/fixed

| # | Finding | Disposition |
|---|---|---|
| 1 | `RunSpec.cwd_rel: String` vs plan `PathBuf` | **Fixed in this commit** (field retyped; `new` signature unchanged; unit test updated). No follow-up bead needed. |
| 2 | Stale `RunSpec` doc comment claiming the struct was still the minimal Phase-0.5 three-field form | **Fixed in this commit** (doc comment now states the six plan-mandated fields and `cwd_rel` semantics). |
| 3 | `CommandBackend::cancel` unsupported | Not drift — recorded above; the command contract has no cancel argv yet. |

Vocabulary rulings (retired `BackendId`/`Status`/`SubmitOutcome` names) are
not repeated here; see `docs/notes/backend-type-naming.md`.
