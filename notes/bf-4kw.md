# Phase 0.5: Walking Skeleton — COMPLETED

Bead: bf-4kw  
Branch: wip/claude-code-glm-4.7-gantry-2/bf-4kw  
Date: 2026-07-28

## Exit Criterion Met

The Phase 0.5 exit criterion is fully satisfied:

✓ **Real cargo test round-trip through shim to local 'remote'**
  - Passing suite returns exit 0
  - Failing suite returns non-zero exit code
  - Decision line prints to stderr: `[gantry] decision:`
  - Verdict trailer prints to stderr: `[gantry] verdict:`
  - Only refs/gantry/* refs created (no branch refs moved)

## Implementation Summary

Phase 0.5 walking skeleton implements the end-to-end pipeline:

1. **argv0 shim dispatch** (`src/main.rs`)
   - Dispatches on argv[0] to cargo tool profile vs management CLI
   - Passthrough fast path for non-intercepted subcommands

2. **GitGate happy path** (`src/gate.rs`)
   - Four eligibility checks: work tree, HEAD, remote, clean tree
   - All checks shell out to system git (no libgit2)

3. **Epoch-ref push** (`src/refs.rs`)
   - Pushes `refs/gantry/<epoch>-<sha>` to ci_remote
   - Ref-mode only (branch mode refused in skeleton)

4. **Command backend** (`src/backend/command.rs`)
   - Local bash executor at `contrib/gantry-exec.sh`
   - submit/wait methods with RunHandle

5. **Decision engine** (`src/decision.rs`)
   - Wires pipeline: GitGate → RefPusher → backend → verdict
   - Returns faithful exit code (Pass → 0, TestFailure → nonzero)

6. **Integration tests** (`tests/integration.rs`)
   - Fixtures for passing and failing suites
   - Runs real gantry binary as cargo shim
   - Verifies round-trip with correct exit codes
   - Asserts only gantry refs created (INV-2)

## Quality Gates Passed

All quality gates pass on the same commit:

```bash
✓ cargo fmt --check
✓ cargo clippy --all-targets -- -D warnings
✓ cargo test (77 unit tests + 4 integration tests)
```

## LOC

Approximately 800 LOC as planned (skeleton allows panics, crude diagnostics).

## Notes

- rust-toolchain.toml pinned at repo root (from scaffold)
- One hardcoded config (`config::Config::hardcoded()`)
- No semaphore, no doctor (Phase 1a scope)
- Parent umbrella bead bf-4kw closes upon successful completion
- Child bead bf-1f9 (stage 5 of 5) already closed

## Ready for Phase 1a

Phase 0.5 exit criterion satisfied. Phase 1a can proceed with:
- Config layering + last-known-good + trust boundary
- Full GitGate with per-check reasons
- RefPusher with leases + GC sweep
- Argo backend
- Complete verdict ladder (incl. gate_failure)
- Write-ahead RunLog
- GANTRY_LOCAL/on/off kill switches
- doctor core checks
