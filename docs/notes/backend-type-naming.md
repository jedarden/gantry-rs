# Backend Core Type Naming — plan §5 surface is canonical

## Decision

The backend core types and trait methods are named exactly as in
**`docs/plan/plan.md` §5 "RemoteBackend trait + implementations"**. The trait
landed in commit **64e81f4** ("Complete bf-2iwc: Define RemoteBackend trait and
basic backend types") using those names, and `src/backend/mod.rs` still ships
them. Several names that appear in **bf-650r's description** are retired
working names from before the plan was finalized. They are **superseded** — do
not reintroduce them, and treat any bead description that uses them as
referring to the plan §5 surface below.

Plan §5 contract (docs/plan/plan.md, §5, trait block):

```rust
trait RemoteBackend {
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError>;
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError>;
    fn wait(&self, h: &RunHandle, deadline: Instant) -> Result<Verdict, BackendError>;
    fn describe(&self, h: &RunHandle) -> String;
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError>;
}
```

## Retired names and their canonical replacements

| Retired name (bf-650r description) | Canonical replacement (plan §5 / shipped code) |
|---|---|
| `BackendId` | **`RunHandle`** — the opaque per-run identifier `submit()` returns and every follow-on call (`wait`, `stream_logs`, `describe`, `cancel`, `status`) takes by reference |
| `SubmitOutcome` | **`Result<RunHandle, BackendError>`** — submission has no dedicated outcome enum; success is the handle, failure is the structured `BackendError` |
| `Status` (the type) | **`Verdict`** for the run's authoritative outcome (`wait()` returns `Result<Verdict, BackendError>`); **`RunStatus`** exists as a separate, coarser type — a non-terminal polling snapshot (`Pending`/`Running`/`Completed`/`Unknown`), never a verdict |
| `status() -> Status` | **`wait(&RunHandle, deadline) -> Result<Verdict, BackendError>`** is the authoritative state query per plan §5. The shipped trait also carries a best-effort `status(&RunHandle) -> Result<RunStatus, BackendError>` (added in gantry-5601513f) for polling, but it returns no verdict — any code expecting `status()` to yield the run's outcome must call `wait()` instead |

There is no type named `Status` in `src/backend/mod.rs`, and a tree-wide grep
finds no `BackendId` or `SubmitOutcome` anywhere in `src/` or the plan. The
full shipped surface around the trait is: `RunSpec`, `RunHandle`, `Verdict`,
`BackendError`, `RunStatus`, `FailureClass`, `VerdictJson` — all consistent
with plan §5 and plan §"Verdict semantics".

## Provenance

- **Canonical spec:** `docs/plan/plan.md` §5 "RemoteBackend trait +
  implementations" (trait signature block), plus §"Data models → RunSpec /
  RunRecord" and §"Verdict semantics (the load-bearing enum)".
- **Landing commit:** `64e81f4` (bf-2iwc) — "Complete bf-2iwc: Define
  RemoteBackend trait and basic backend types" — defined the trait with these
  exact names and completed `RunSpec`'s six fields
  (`tool`, `subcommand`, `args`, `repo_url`, `sha`, `cwd_rel`).
- **Corroborating decision:** DD-5 in `docs/notes/design-decisions.md`
  ("Backend abstraction with `command` as the universal backend") already
  states the trait in plan terms: `submit(RunSpec) -> RunHandle`,
  `wait(&RunHandle) -> Verdict`, `cancel(&RunHandle)`.

## Why this note exists

bf-650r ("Define RemoteBackend trait and core backend types") was written
before the trait landed and its description still names `BackendId`,
`Status`, `SubmitOutcome`, and `status() -> Status`. A worker picking the bead
up from its description alone sees vocabulary that contradicts both the plan
and the shipped code — the exact path by which types that contradict the plan
get reintroduced. If you are working bf-650r or any backend work and the
description's names don't match `src/backend/mod.rs`, the plan §5 names win;
this note is the recorded ruling.

## Guidance for future workers

- Never create `BackendId`, `Status`, or `SubmitOutcome`. If a bead or note
  uses one of those names, substitute the canonical replacement above.
- A handle is `RunHandle`; a terminal outcome is `Verdict`; a mid-flight
  snapshot is `RunStatus`; a backend failure is `BackendError`.
- `wait()` is the only verdict authority. `status()` is best-effort polling
  and must keep returning `RunStatus`, never a verdict.
- If bf-650r is ever rescoped or its description rewritten, update it to the
  plan §5 vocabulary and cite this note.
