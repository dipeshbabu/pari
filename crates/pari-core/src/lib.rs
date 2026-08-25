#![forbid(unsafe_code)]
//! Core similarity primitives for Pari.
//!
//! `pari-core` owns hashing, signature construction, similarity estimation,
//! and compatibility validation. Storage and language bindings live in
//! separate crates so the core remains usable as a standalone Rust library.

mod hash;
mod minhash;

pub use hash::{sha1_hash32, sha1_hash64};
pub use minhash::{MinHash32, MinHash64, MinHashError, AFFINE32_SCHEME, AFFINE64_SCHEME};

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
