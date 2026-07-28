//! Integration test fixture for gantry cargo shim
//!
//! This crate contains two test suites:
//! - `passing` - all tests pass
//! - `failing` - at least one test fails

// Passing test suite
mod passing {
    #[cfg(test)]
    mod tests {
        #[test]
        fn test_addition() {
            assert_eq!(2 + 2, 4);
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
}

// Failing test suite
mod failing {
    #[cfg(test)]
    mod tests {
        #[test]
        fn test_passing_always() {
            assert_eq!(1 + 1, 2);
        }

        #[test]
        fn test_intentional_failure() {
            // This test intentionally fails
            assert_eq!(1 + 1, 3, "This test is designed to fail");
        }
    }
}
