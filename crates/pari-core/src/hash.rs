use sha1::{Digest, Sha1};

/// Hash bytes to a 32-bit value using the first four bytes of a SHA-1 digest
/// interpreted in little-endian order.
///
/// This byte interpretation matches `ekzhu/datasketch`'s `sha1_hash32` helper
/// so applications migrating pre-hashed tokens can preserve the input hash
/// stage even though Pari uses its own permutation seed mapping.
#[must_use]
pub fn sha1_hash32(data: &[u8]) -> u32 {
    let digest = Sha1::digest(data);
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// Hash bytes to a 64-bit value using the first eight bytes of a SHA-1 digest
/// interpreted in little-endian order.
///
/// This byte interpretation matches `ekzhu/datasketch`'s `sha1_hash64` helper.
#[must_use]
pub fn sha1_hash64(data: &[u8]) -> u64 {
    let digest = Sha1::digest(data);
    u64::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

#[cfg(test)]
mod tests {
    use super::{sha1_hash32, sha1_hash64};

    #[test]
    fn sha1_helpers_match_datasketch_byte_order() {
        assert_eq!(sha1_hash32(b"hello"), 499_578_026);
        assert_eq!(sha1_hash64(b"hello"), 11_738_849_977_924_252_842);
        assert_eq!(sha1_hash32(b"pari"), 1_875_174_344);
        assert_eq!(sha1_hash64(b"pari"), 9_885_447_646_909_621_192);
    }
}
