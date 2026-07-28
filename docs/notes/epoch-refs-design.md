# Epoch refs design note

*Plan R2 resolution: epoch-vs-content-addressing tension (Q-3). Settled before Phase 1a RefPusher implementation (bf-3v5).*

## The tension

RefPusher creates hidden refs on a git remote to ship commits to the remote executor. The ref naming scheme carries two competing goals:

1. **Content addressing**: The same commit SHA pushed at different times should resolve to a single ref, avoiding duplication.
2. **Epoch-based GC**: Refs must carry enough metadata to enable garbage collection without a daemon or cross-box coordination.

The epoch-prefix scheme `refs/gantry/<epoch>-<sha>` resolves this tension in favor of *epoch-based GC*, accepting that identical SHAs may exist under multiple refs at different times.

## Why epoch refs

### Problem statement

We need a GC mechanism for `refs/gantry/*` that satisfies:

- **No daemon**: No cron job, no background process (plan §4).
- **No cross-box coordination**: Each gantry instance must operate independently (EC-09).
- **No race with in-flight runs**: A ref needed by a running workflow must never be deleted mid-run.
- **Terminal-only cleanup**: Only refs from completed runs are eligible; in-flight runs retain their refs.

### The epoch prefix solution

Ref names carry their creation time in the epoch component:

```
refs/gantry/1720441234-abc123def456...
           ^----epoch----^ ^----SHA----^
```

**Epoch** = Unix timestamp (seconds) at push time.

This enables the opportunistic sweep (plan §4):

1. Every successful push *also* queries `ls-remote` for all `refs/gantry/*` refs.
2. For each ref: extract its epoch from the ref name.
3. Delete iff `epoch < now_sweep_threshold` AND the local run record is terminal (pass/fail/gate_failure).
4. Each box sweeps only refs it created (lease records are local; see EC-09).

No daemon, no coordination, no races: a ref whose epoch is "recent enough" is always kept, regardless of which box created it. The lease only matters for borderline cases near the sweep threshold.

## The content-addressing tradeoff

### What we lose

The same commit SHA pushed at time T₁ and again at time T₂ creates two refs:

```
refs/gantry/1720441200-abc123...  (push #1)
refs/gantry/1720444800-abc123...  (push #2, same SHA, later epoch)
```

This is *not* pure content addressing. Identical content can exist under multiple ref names.

### Why this is acceptable

1. **Refs are cheap**: Git ref storage is minimal (~40 bytes per ref). Even at fleet scale (1000 runs/day, 7-day retention), this is ~280KB of ref storage — negligible compared to the repo size.

2. **Refs are hidden**: `refs/gantry/*` never appear in `git branch` output and are not user-visible. They're implementation details, not branch names.

3. **Refs are transient**: The epoch sweep deletes refs older than the retention window (typically 7 days). Duplication is bounded and short-lived.

4. **No coordination needed**: Each box can safely sweep refs it created without consulting other boxes. A ref created by box A at epoch E₁ is never touched by box B's sweep — only box A sweeps it (when box A's next run happens to trigger the sweep).

5. **Invariants preserved**: The critical invariant is "no branch ref ever moves" (INV-2). Epoch refs satisfy this: they're refs under `refs/gantry/*`, never `refs/heads/*`. Branch refs are untouched.

## Alternatives considered

### Alternative 1: Pure content addressing (`refs/gantry/<sha>`)

- **Problem**: No epoch information means we can't implement "delete refs older than N days" without external metadata.
- **We'd need**: A separate timestamp store (file, database, git note) that maps sha → push time.
- **Why rejected**: External metadata is fragile (state drift, cross-box coordination, recovery after corruption). The epoch prefix carries the metadata *in the ref name itself*.

### Alternative 2: Branch refs (`refs/heads/gantry/<sha>`)

- **Problem**: Violates INV-2 ("no branch ref ever moves") and security consideration S-1 ("no implicit branch pushes").
- **Why rejected**: The predecessor's worst incident class was accidental branch pushes. Hidden refs under `refs/gantry/*` are safe; branch refs are not.

### Alternative 3: Cross-box coordination via shared state

- **Problem**: Requires a shared database or centralized GC service.
- **Why rejected**: Violates "no daemon," adds operational complexity, creates a single point of failure. The epoch prefix is *fully distributed*.

## Lease records (EC-09)

### What is a lease?

A lease is a client-side record that protects a ref while a run is in-flight:

```json
{
  "ref": "refs/gantry/1720441234-abc123...",
  "run_id": "01J...",
  "created_at": 1720441234,
  "expires_at": 1720444834  // created_at + lease_duration (default 1 hour)
}
```

Stored in the state dir (`~/.local/state/gantry/leases.jsonl`), one record per push.

### Why leases exist

The epoch sweep must never delete a ref that an in-flight run still needs. Leagues solve this:

- When pushing: append a lease record with `expires_at = now + lease_duration`.
- When sweeping: *only* delete refs whose epoch is expired **AND** whose lease has expired.
- When a run finishes: mark its lease as "terminal" (the run ended, but the ref is kept for the retention window).

### Cross-box independence (EC-09)

**Lease records are local to each box.** Box A does not see Box B's leases, and vice versa. This is intentional:

- Each box sweeps only refs it created (tracked via local lease records).
- A ref from Box A is never touched by Box B's sweep.
- No coordination needed: the `ls-remote` output is shared, but the *decision* to delete is local.

This means:
- If the same SHA is pushed by two boxes at different times, two refs exist. Each box GCs its own ref independently.
- No centralized state, no distributed consensus, no "who owns this ref?" ambiguity.

### Lease expiry and retention

Two time bounds:

1. **Lease duration** (default: 1 hour): How long a ref is protected while a run is in-flight. If a run takes longer than 1 hour, the lease is extended on-demand.
2. **Retention window** (default: 7 days): How long terminal refs are kept after the run completes.

The sweep deletes refs whose epoch is older than the retention window **AND** whose lease is expired. Terminal refs (run finished) are kept for the retention window, then deleted.

## Sweep threshold calculation

The sweep operates on a time threshold derived from the retention window:

```rust
let now = epoch_now();
let sweep_threshold = now - RETENTION_WINDOW_SECONDS;  // e.g., 7 days ago

// Delete iff: epoch < sweep_threshold AND lease expired
for ref in ls_remote("refs/gantry/*") {
    let epoch = extract_epoch_from_ref_name(&ref);
    if epoch < sweep_threshold && lease_is_expired(&ref) {
        git push remote :<ref>;  // delete
    }
}
```

This ensures:
- Recent refs (epoch >= sweep_threshold) are never touched, regardless of lease state.
- Old refs (epoch < sweep_threshold) are deleted only if their lease is expired.
- In-flight runs retain their refs even if the epoch is old (lease is not expired).

## Edge cases

### EC-09: Same sha pushed by two boxes in the same epoch window

If two boxes push the same SHA within the same epoch window (e.g., within the same second), the ref names collide: both push `refs/gantry/1720441234-abc123...`.

**Behavior**: The second push is a no-op (git push with the same ref SHA is idempotent). Both boxes record separate leases, but they refer to the same ref on the remote.

**Cleanup**: When the first box sweeps this ref, it deletes it from the remote. The second box's lease now points to a deleted ref — but this is benign: the run is already terminal, so the ref is no longer needed. The second box's next sweep simply sees "ref not found" and continues.

### Clock skew between boxes

Different boxes may have slightly unsynchronized clocks (seconds-level skew). This is acceptable:

- The retention window (7 days) is large compared to typical clock skew (<1s).
- A ref deleted "too early" by a fast-clock box is not a correctness issue: the run is terminal, so the ref is no longer needed.
- A ref deleted "too late" by a slow-clock box is also acceptable: it just means a few extra refs linger until the next sweep.

### Shard-scoped refs vs. global refs

The design uses *local lease records*, which means each box GCs only its own refs. This is a tradeoff:

- **Pro**: No coordination, simple, robust.
- **Con**: If box A dies and never sweeps again, its refs persist until another mechanism cleans them up (e.g., server-side ref expiration, if available).

**Acceptance**: This is deemed acceptable for v1. The refs are hidden, cheap, and bounded (7-day retention). If a box dies, its refs age out naturally. A future v1.x enhancement could add a "global sweep" mode for clusters with a dedicated GC service.

## Implementation plan (Phase 1a, bf-3v5)

1. **RefPusher::push** enhancement:
   - Add client-side lease record creation on push.
   - Add opportunistic sweep after successful push.
   - Add `push_mode = "branch"` legacy support with warning.
   - Map push failures to `InfraFailure` (plan §4).

2. **Lease storage**:
   - Append-only JSONL file in state dir (`~/.local/state/gantry/leases.jsonl`).
   - One record per push: `{"ref": "...", "run_id": "...", "created_at": ..., "expires_at": ..., "terminal": false}`.
   - Terminal runs marked `terminal: true` when verdict is recorded.

3. **Sweep logic**:
   - Run `git ls-remote <ci_remote> "refs/gantry/*"` to list all refs.
   - Extract epoch from each ref name.
   - For each ref, check local lease state.
   - Delete iff `epoch < sweep_threshold` AND `lease.expired` AND (if `!terminal`, `lease.expires_at < now`).
   - Each box only sweeps refs it has leases for (EC-09).

4. **Config**:
   - Add `push_mode` field (`"ref"` default, `"branch"` legacy with warning).
   - Add `lease_duration_seconds` (default 3600 = 1 hour).
   - Add `retention_days` (default 7).

5. **Testing**:
   - Unit test for epoch extraction from ref name.
   - Unit test for sweep threshold calculation.
   - Integration test: push → lease created → sweep deletes old ref.
   - Integration test: same SHA, two epochs → two refs → both swept independently.
   - Property test: ref name fuzzing ensures epoch is always extractable.

## Summary

**Decision**: Epoch refs (`refs/gantry/<epoch>-<sha>`) are the chosen design.

**Rationale**:
- Enables GC without daemon, coordination, or external metadata.
- Accepts content-addressing tradeoff (duplicate refs for same SHA) as cheap and harmless.
- Preserves critical invariants (no branch mutation, hidden refs).
- Lease records are local (EC-09), enabling fully distributed GC.

**Alternatives rejected**: Pure content addressing (requires external metadata), branch refs (violates INV-2), cross-box coordination (violates "no daemon").

**Phase 1a acceptance**: RefPusher with leases + opportunistic sweep, push failure → InfraFailure, `push_mode=branch` legacy support with warning.
