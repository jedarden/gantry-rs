# bf-28dv: Passthrough exec and exit-code fidelity verification

## Summary
Bead bf-28dv covers the passthrough fast path in src/shim.rs. Upon inspection, the implementation is already complete and all acceptance criteria are met.

## Implementation Status

### passthrough function (src/shim.rs:385-430)
The `passthrough` function is fully implemented with:
- Real binary resolution via `resolve_real_binary` (includes self-recursion guard)
- Error handling: prints loud `[gantry]` message and returns non-zero ExitCode on failure
- Spawns resolved binary with `argv[1..]` forwarded byte-exact via `Command::args`
- Exit-code fidelity via `status_to_exit_code` helper

### Acceptance Criteria Verification
1. ✅ **Exit code matching**: Tests verify passthrough returns ExitCode::SUCCESS for child exit 0, and ExitCode::from(42) for child exit 42
2. ✅ **Byte-exact argv forwarding**: Test `passthrough_forwards_pathological_argv_byte_exact` verifies pathological args (spaces, quotes, newlines, ~100KB arg) round-trip unchanged
3. ✅ **Self-recursion guard**: Test `passthrough_degrades_loudly_without_spawning_when_the_override_is_the_gantry_binary` verifies infinite loop protection
4. ✅ **main.rs compatibility**: The function signature `pub fn passthrough(cfg: &crate::config::Config, argv: &[String]) -> std::process::ExitCode` is ready for main.rs to call when it's implemented (forward-looking criterion)
5. ✅ **Quality gates**: All pass
   - `cargo fmt --check`: ✅
   - `cargo clippy --all-targets -- -D warnings`: ✅
   - `cargo test`: ✅ (44 tests pass, including 6 passthrough-specific tests)

## Test Coverage
The implementation includes comprehensive tests:
- `passthrough_runs_the_resolved_binary_and_returns_success` - happy path
- `passthrough_propagates_a_nonzero_exit_code_verbatim` - exit code 42 fidelity
- `passthrough_propagates_a_failure_exit_code_as_nonzero` - exit code 1
- `passthrough_propagates_signal_death_as_128_plus_signal` - Unix signal death (143 for SIGTERM)
- `passthrough_forwards_pathological_argv_byte_exact` - byte-exact argv round-trip
- `passthrough_degrades_loudly_without_spawning_when_the_override_is_the_gantry_binary` - guard against infinite loop

## Conclusion
No code changes were required - the passthrough implementation is complete, tested, and ready for integration with main.rs when that component is implemented.
