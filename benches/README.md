# Gantry Benchmarks

This directory contains performance benchmarks for gantry.

## Passthrough Benchmark (`passthrough.sh`)

Measures the overhead of gantry's passthrough fast path, enforcing the INV-4 requirement: **<5ms p99 overhead**.

### Usage

```bash
./benches/passthrough.sh
```

### What it tests

The benchmark compares:
1. **Real cargo --version** (baseline): Direct execution of the real cargo binary
2. **Gantry passthrough**: Execution through gantry with `GANTRY_LOCAL=1` (forces passthrough path)

The passthrough path includes:
- argv[0] dispatch
- Config loading (mmap'd)
- Real binary resolution (with self-recursion guard)
- Exec to real binary

### Requirements

- `hyperfine` (install via `cargo install hyperfine`)
- Real cargo binary (from rust toolchain)
- Gantry built and available in PATH

### CI Integration

This benchmark should run in CI to enforce the <5ms p99 budget. If the regression exceeds the target, the build should fail.

### Expected Results

- **Mean**: <2ms
- **p99 (max)**: <5ms
- **Overhead**: <1ms compared to direct execution

Results may vary based on:
- Filesystem performance (config read)
- PATH length (resolution walk)
- System load
