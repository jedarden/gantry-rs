# bf-6d4: Shim Dispatch, Passthrough, and Hardcoded Config

## Summary

All acceptance criteria for Phase 0.5 walking skeleton stage 1 have been verified. The implementation was already complete from prior bead work (bf-2w7 and related beads in the bf-614q chain).

## Implementation Status

### Core Modules (Complete)
- **src/main.rs**: argv[0] dispatch implemented with cargo/gantry/unknown profiles
- **src/config.rs**: Hardcoded Config struct with intercepts(), ci_remote, deadline()
- **src/shim.rs**: invocation_name(), resolve_real_binary(), passthrough() with self-recursion guard

### Acceptance Criteria Verified

✅ **Compilation**: `cargo build` succeeds with only main.rs + shim.rs + config.rs present
- No gate/refs/backend/local/runlog modules invoked or stubbed
- Clean compilation with no dead_code warnings

✅ **Passthrough**: Non-intercepted subcommands (e.g., `cargo version`) forward untouched to real cargo
- Self-recursion guard prevents infinite re-exec
- PATH resolution strips shim directory
- Exit code fidelity (INV-3) verified in tests

✅ **GANTRY_LOCAL=1**: Forces passthrough even for intercepted subcommands (`cargo test`)
- Environment variable checked in run_cargo_profile()
- Unit tests verify the logic path

✅ **Management CLI**: `gantry version` prints version line correctly
- Outputs: "gantry 0.1.0"
- Help system implemented for `gantry help`

✅ **No Pipeline Code**: GitGate/RefPusher/backend modules not invoked
- Phase 0.5 is passthrough-only as specified
- Remote offload placeholder in place but never reached

✅ **Quality Gates**: All pass on same commit
```bash
cargo fmt --check          # ✓ No formatting issues
cargo clippy --all-targets -- -D warnings  # ✓ No warnings
cargo test                 # ✓ 62 tests passed
```

## Test Coverage

- **44 unit tests** in shim.rs covering path resolution, self-recursion guard, passthrough
- **5 unit tests** in config.rs covering hardcoded config behavior
- **13 unit tests** in main.rs covering dispatch logic and CLI commands

Key test scenarios:
- Exit code fidelity (0, 1, 42, signal death)
- Pathological argv round-trip (100KB args, newlines, shell metachars)
- Self-recursion guard (override, symlink, PATH lookup)
- PATH stripping and directory resolution
- GANTRY_LOCAL=1 forced passthrough logic

## Architecture Notes

The walking skeleton correctly implements the three-layer dispatch:
1. **argv[0] dispatch** (main.rs) → cargo vs gantry profile
2. **Subcommand interception** (config.rs) → test vs passthrough
3. **Binary resolution** (shim.rs) → override vs PATH lookup with self-recursion guard

This establishes the foundation for Phase 1a remote offload while maintaining passthrough semantics for non-intercepted commands.
