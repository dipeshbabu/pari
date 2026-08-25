#![forbid(unsafe_code)]
//! Core similarity primitives for Pari.
//!
//! This crate intentionally starts small. Algorithms, indexes, and storage
//! abstractions are added behind focused issues with correctness tests and
//! benchmark evidence.

/// Human-readable engine name.
pub const ENGINE_NAME: &str = "Pari";

#[cfg(test)]
mod tests {
    use super::ENGINE_NAME;

    #[test]
    fn engine_name_is_stable() {
        assert_eq!(ENGINE_NAME, "Pari");
    }
}
