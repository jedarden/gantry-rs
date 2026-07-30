//! `passing-suite` — one of the two gantry integration test fixtures (bf-1hgq).
//!
//! Used by `tests/integration.rs` as a real, standalone cargo project: the test
//! harness `git init`s this directory, symlinks the built gantry binary in as
//! `cargo`, and runs `cargo test` through the shim. Every test here passes, so
//! the round trip is expected to exit 0.
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
    fn test_addition() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    fn test_string_concat() {
        assert_eq!("hello".to_owned() + " world", "hello world");
    }

    #[test]
    fn test_vec_operations() {
        let vec = vec![1, 2, 3];
        assert_eq!(vec.len(), 3);
        assert_eq!(vec[0], 1);
    }
}
