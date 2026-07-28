# bf-53a: RefPusher epoch-ref push implementation verification

## Summary

The RefPusher epoch-ref push functionality was already fully implemented in `src/refs.rs`. This document confirms that the implementation meets all acceptance criteria for bead bf-53a.

## Implementation verified

### Core functionality (src/refs.rs)

**RefPusher::push(config, sha)** - Lines 84-108
- Generates epoch prefix using `epoch_now()` (unix seconds)
- Builds ref name: `refs/gantry/<epoch>-<sha>`
- Executes `git push <ci_remote> <sha>:<ref_name>` via `run_git_push()`
- Returns `PushResult` with success/failure status

**Key components:**
- `epoch_now()`: Returns current unix timestamp in seconds (line 140-145)
- `run_git_push()`: Shells out to git push command (line 162-176)
- `assert_ref_mode_only()`: Documents ref-mode-only promise (line 121-129)

### Acceptance criteria verification

#### 1. ✅ Pushing HEAD to local bare remote creates refs/gantry/<epoch>-<sha>
**Test:** `push_creates_epoch_ref_on_remote` (line 294-336)
- Creates a local bare repo as remote
- Pushes HEAD via RefPusher::push()
- Verifies the ref appears in `git ls-remote` output
- Asserts ref name contains both SHA and `refs/gantry/` prefix

#### 2. ✅ No branch ref (refs/heads/*) moves during push
**Test:** `push_does_not_modify_branch_refs` (line 339-384)
- Captures `git ls-remote` before and after push
- Asserts no `refs/heads/` refs exist in either output
- Confirms the never-mutate-branch-state promise

#### 3. ✅ Push to nonexistent/unfetchable remote errors loudly
**Test:** `push_to_nonexistent_remote_fails` (line 387-409)
- Uses config with nonexistent remote name
- Verifies push returns `success=false`
- Asserts failure reason is non-empty

#### 4. ✅ Unit tests with local bare repo and ref name pattern assertion
All tests use `setup_repo_with_remote()` (line 249-291) which:
- Creates a temporary bare git repo as remote
- Initializes a main repo with origin pointing to bare repo
- Creates an initial commit and returns its SHA
- Tests verify both ref name pattern and branch state preservation

#### 5. ✅ Quality gates pass (same commit)

```bash
cargo test                          # 62 tests passed
cargo fmt --check                   # No formatting issues
cargo clippy --all-targets -- -D warnings  # No warnings
cargo build                         # Builds successfully
```

## Design notes

### Ref-mode-only enforcement
The `assert_ref_mode_only()` function documents the skeleton's ref-mode-only promise. Phase 1a (bf-3v5) may add `push_mode` config support for the legacy branch mode escape hatch.

### GC and leases
The epoch prefix serves as the GC lever. Leases and the GC sweep are explicitly deferred to Phase 1a (bf-3v5) as noted in the code comments.

### Integration
This implementation builds on:
- Stage 1 (bf-3pr): shim/config
- Stage 2 (bf-3a7): GitGate

And is consumed by:
- Stage 4 (bf-2vr): command backend
- Stage 5 (bf-1f9): end-to-end pipeline (exit criterion)

## Conclusion

The RefPusher implementation in `src/refs.rs` fully satisfies all acceptance criteria for bead bf-53a. All unit tests pass, quality gates are satisfied, and the code correctly implements the epoch-ref push pattern as specified in the plan (Component 4: RefPusher).
