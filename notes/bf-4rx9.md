# bf-4rx9 — Full verdict ladder with retry semantics

Plan refs: §"Verdict semantics", DD-4, INV-7, INV-3.

## What the ladder is

| Verdict | Exit code | Local rerun? |
|---|---|---|
| `Pass` | 0 | no |
| `TestFailure { exit_code }` | preserved (≥1) | **never** (INV-7) |
| `GateFailure { gate_name, exit_code }` | preserved (≥1) | **never** (INV-7) |
| `InfraFailure { reason }` | 1, or the local run's code | yes — capped |
| `Cancelled` | 130 | no |
| `Superseded { newer_sha }` | 0 | no |

`decide_retry(verdict, fallbacks_used, policy) -> RetryDecision` is a pure
function, so INV-7 is asserted over the whole (verdict × budget) space rather
than reviewed at each call site: the match has no arm that can route a
`TestFailure` or `GateFailure` to `RetryDecision::LocalFallback`.

`FallbackPolicy` carries the *attempt* cap (`DEFAULT_MAX_LOCAL_FALLBACKS = 1`,
read from `Config::fallback_policy()`), so a backend outage buys exactly one
honest local answer and can never become a retry loop. With the cap spent, the
decision is `FallbackExhausted` — loud and non-zero, because an absent verdict
may never surface as success.

## Backend status → verdict

The verdict comes from the backend's status surface, never from parsing log text
(INV-3). Two shapes, in precedence order:

1. **verdict.json** (`VerdictJson`, schema_version 1) — `oom` / `deadline_exceeded`
   → `InfraFailure`; any `failure_class` → `TestFailure`; `gate_failed` (with no
   failure class) → `GateFailure` attributed to that gate; otherwise phase +
   exit code. A well-formed verdict.json this client cannot interpret (newer
   schema) is contract drift → `InfraFailure`, never a guessed verdict.
2. **Exit code only** — the universal contract for plain command templates:
   0 → Pass, 1 → TestFailure, ≥2 (or a signal-killed executor) → InfraFailure.
   A run that never happened is never reported as a test failure (DD-4).

`CommandBackend::wait` prefers a verdict.json on stdout over the exit code;
`ArgoBackend::wait` reads it from the workflow's output parameters.

## Tests

- `src/decision.rs` — one test per rung, the INV-7 property over
  (verdict × used × cap), the cap/exhaustion behavior, and verdict.json →
  decision cases.
- `src/backend/command.rs` — the executor status contract (exit codes,
  verdict.json precedence, gate attribution, schema drift, chatter).
- `tests/verdict_ladder.rs` (new) — the whole path across the public API:
  a real executor script through `submit` → `wait` → `decide_retry`, covering
  every rung plus the INV-7 property driven from backend status.

142 tests pass; `cargo fmt --check` and clippy are clean for these files.

## Test-suite reliability fixed along the way

The suite was flaky in ways that made the ladder unverifiable:

- `gate` and `refs` each held their own cwd mutex, so they serialized against
  themselves but not each other — one module deleting a temp dir it had chdir'd
  into made the other's `Command::spawn` fail with ENOENT. Both now take one
  process-wide, poison-tolerant lock (`crate::testutil::lock_cwd`).
- The command-backend fixtures are executed as `bash <script>` instead of
  directly: exec'ing a file another thread still holds open for writing fails
  with ETXTBSY.
- `tests/integration.rs` round-trips share mutable fixture directories (they
  re-create the `cargo` symlink and re-init the git repo), so they now run under
  one lock.

## Not in this bead

- `Cancelled` and `Superseded` are ladder rungs with no producer yet — Ctrl-C
  propagation and supersede-on-newer-sha are their own later beads. Both are
  covered at the decision layer.
- The *resource* cap on the local rung (cgroup slice, DD-9) and the fallback
  admission semaphore hook into `run_local` in Phase 1b (bf-xj0, bf-299).
