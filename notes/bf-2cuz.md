# bf-2cuz — Wire resolve_real_binary() top-level resolver with env-injected tests

`bf-2cuz` is the **umbrella** bead (label `umbrella`) for the
`resolve_real_binary` integration — child 4 of the `bf-614q` auto-split, the
piece that chains after the `find_in_path` helper and *produces*
`resolve_real_binary` (unblocking `bf-1m69` re-exec guard and `bf-28dv`
passthrough). It was itself auto-split: its child beads landed the function
body and each test independently on this shared wip branch. This run produced
no source changes — it verified the assembled work and closed the umbrella.

## What the bead asked for (src/shim.rs)

The public resolver composing the three private helpers, plus three
env-injected integration tests:

- `resolve_real_binary(cfg: &Config) -> Result<PathBuf, String>`:
  1. `cfg.real_binary == Some(path)` → `Ok(path)` with **no** PATH search.
  2. else → `shim_dir()?` → read `PATH` → `strip_dir_from_path(path, shim)` →
     `find_in_path("cargo", stripped)`; return its `Ok` or propagate its `Err`.
  - Never silently falls through to self or `None`.
- Tests: (a) override `Some(/usr/bin/cargo)` → `Ok(/usr/bin/cargo)`; (b) a PATH
  of ONLY the shim dir → `Err`, not the gantry binary (self-recursion prevented
  at the resolution layer); (c) a real PATH with a genuine cargo → `Ok(real)`.

## State found on entry — all landed by child beads

- `resolve_real_binary` body — child `bf-xsv8`, commit `5d11b20`
  ("Add resolve_real_binary() public resolver composing helpers").
- PATH-mutating test harness + override short-circuit test (a) — child
  `bf-68qu`, commit `3eaa8bc`.
- Self-recursion-guard test (b) — child `bf-518l`, commit `28c5025`.
- Happy-path test (c) — child `bf-571j`, commit `c9d8348`.

Dependencies `bf-5os5` (`find_in_path`) and `bf-571j` (happy-path test) are both
`closed`. The required function and all three tests are present and correct in
`src/shim.rs`; nothing remained to author.

## Divergence resolved this run

Local and remote had diverged 1/1 at merge-base `c9d8348` on a *content-identical*
`bf-571j` checkpoint commit (`3b03d33` local vs `d65b030` remote — same tree,
different object hashes). Confirmed byte-identical trees, then `git reset --hard`
to the remote tip (no content lost, no force-push) — the
[[auto-split-shared-branch-push-divergence]] pattern: all bf-2cuz children share
one wip branch and the parent's checkpoint makes a worker's push
non-fast-forward.

## Verification (all green)

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — no warnings.
- `cargo test` — 34 passed, 0 failed, including the three acceptance tests:
  `resolve_real_binary_returns_override_verbatim_with_empty_path`,
  `resolve_real_binary_self_recursion_guard_yields_err_when_path_is_only_shim_dir`,
  `resolve_real_binary_finds_real_cargo_on_inherited_path`. The PATH-mutating
  tests serialize behind `ENV_LOCK` with PATH restored on drop, so no synthetic
  PATH leaks into siblings.

## Outcome

Source unchanged and already committed/pushed by the child beads; this run lands
only this summary note. Bead closed via `br close bf-2cuz`.

## Note on lib.rs / Cargo.lock

`src/lib.rs` (`pub mod config; pub mod shim;`) and `Cargo.lock` remain
intentionally untracked — the crate root is the `bf-2w7` skeleton-wiring bead's
deliverable, outside bf-2cuz's `src/shim.rs` scope. Every sibling bead in this
chain has run the quality gates with `lib.rs` present on disk but left it
uncommitted; this umbrella follows that convention. The on-disk crate compiles
and passes all gates, satisfying the "compiles with config.rs + the three child
helpers" criterion.
