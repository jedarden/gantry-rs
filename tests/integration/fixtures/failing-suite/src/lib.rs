//! `failing-suite` — one of the two gantry integration test fixtures (bf-1hgq).
//!
//! Used by `tests/integration.rs` as a real, standalone cargo project: the test
//! harness `git init`s this directory, symlinks the built gantry binary in as
//! `cargo`, and runs `cargo test` through the shim. At least one test here
//! intentionally fails, so the round trip is expected to exit non-zero.
//!
//! Kept deliberately tiny and dependency-free so the fixture builds fast and
//! offline.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passing_always() {
        assert_eq!(add(1, 1), 2);
    }

    #[test]
    fn test_intentional_failure() {
        // This test intentionally fails so the fixture's round trip exits non-zero.
        assert_eq!(1 + 1, 3, "This test is designed to fail");
    }
}
