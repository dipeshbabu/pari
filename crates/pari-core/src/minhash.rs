use std::{error::Error, fmt};

use crate::hash::{sha1_hash32, sha1_hash64};

/// Stable identifier for Pari's 32-bit affine `MinHash` scheme.
pub const AFFINE32_SCHEME: &str = "pari-affine32-v1";
/// Stable identifier for Pari's 64-bit affine `MinHash` scheme.
pub const AFFINE64_SCHEME: &str = "pari-affine64-v1";

/// Errors returned by `MinHash` construction and compatibility checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinHashError {
    /// The requested permutation count is zero or cannot be represented by the
    /// stable on-disk compatibility metadata planned for Pari.
    InvalidPermutationCount { requested: usize },
    /// Two sketches use different seeds and therefore different permutation
    /// families.
    IncompatibleSeed { left: u64, right: u64 },
    /// Two sketches contain different numbers of permutations.
    IncompatiblePermutationCount { left: usize, right: usize },
}

impl fmt::Display for MinHashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPermutationCount { requested } => write!(
                formatter,
                "num_perm must be in 1..={}, got {requested}",
                u32::MAX
            ),
            Self::IncompatibleSeed { left, right } => {
                write!(formatter, "incompatible MinHash seeds: {left} != {right}")
            }
            Self::IncompatiblePermutationCount { left, right } => write!(
                formatter,
                "incompatible MinHash permutation counts: {left} != {right}"
            ),
        }
    }
}

impl Error for MinHashError {}

/// A 32-bit `MinHash` using affine permutations modulo `2^32`.
///
/// The permutation construction is derived from datasketch 2.x's `affine32`
/// design: inputs are pre-mixed with the `MurmurHash3` finalizer, then mapped by
/// `a * h + b` with wrapping arithmetic and odd multipliers. Pari deliberately
/// uses a separately specified `SplitMix64` seed mapping, so equal seeds do not
/// imply byte-identical signatures with datasketch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinHash32 {
    seed: u64,
    hashvalues: Vec<u32>,
    multipliers: Vec<u32>,
    offsets: Vec<u32>,
}

impl MinHash32 {
    /// Create an empty sketch with `num_perm` deterministic permutations.
    pub fn new(num_perm: usize, seed: u64) -> Result<Self, MinHashError> {
        validate_num_perm(num_perm)?;

        let mut generator = SplitMix64::new(seed);
        let mut multipliers = Vec::with_capacity(num_perm);
        let mut offsets = Vec::with_capacity(num_perm);

        for _ in 0..num_perm {
            multipliers.push(generator.next_u32() | 1);
        }
        for _ in 0..num_perm {
            offsets.push(generator.next_u32());
        }

        Ok(Self {
            seed,
            hashvalues: vec![u32::MAX; num_perm],
            multipliers,
            offsets,
        })
    }

    /// Reconstruct a sketch from an already computed Pari affine32 signature.
    ///
    /// The supplied `seed` recreates the deterministic permutation metadata so
    /// the returned sketch remains safe to update or merge after loading. The
    /// signature length defines `num_perm` and must satisfy the same bounds as
    /// [`Self::new`]. Callers are responsible for ensuring the values were
    /// produced by Pari's [`AFFINE32_SCHEME`] with the same seed.
    pub fn from_signature(signature: Vec<u32>, seed: u64) -> Result<Self, MinHashError> {
        let mut sketch = Self::new(signature.len(), seed)?;
        sketch.hashvalues = signature;
        Ok(sketch)
    }

    /// Update the sketch with one byte string using Pari's default SHA-1 input
    /// hash and 32-bit affine permutation family.
    pub fn update(&mut self, value: &[u8]) {
        self.update_hashed(sha1_hash32(value));
    }

    /// Update the sketch with many values without allocating per permutation.
    pub fn update_many<I, T>(&mut self, values: I)
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        for value in values {
            self.update(value.as_ref());
        }
    }

    /// Estimate Jaccard similarity with another compatible sketch.
    pub fn jaccard(&self, other: &Self) -> Result<f64, MinHashError> {
        self.ensure_compatible(other)?;
        Ok(signature_match_ratio(&self.hashvalues, &other.hashvalues))
    }

    /// Merge another compatible sketch into this one, representing the union
    /// of the two source sets.
    pub fn merge(&mut self, other: &Self) -> Result<(), MinHashError> {
        self.ensure_compatible(other)?;
        for (current, incoming) in self.hashvalues.iter_mut().zip(&other.hashvalues) {
            *current = (*current).min(*incoming);
        }
        Ok(())
    }

    /// Reset the sketch to its empty state without rebuilding permutations.
    pub fn clear(&mut self) {
        self.hashvalues.fill(u32::MAX);
    }

    /// Return whether no values have been added since construction or reset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashvalues.iter().all(|value| *value == u32::MAX)
    }

    /// Borrow the signature values.
    #[must_use]
    pub fn signature(&self) -> &[u32] {
        &self.hashvalues
    }

    /// Return the deterministic permutation seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the number of permutation values in the signature.
    #[must_use]
    pub fn num_perm(&self) -> usize {
        self.hashvalues.len()
    }

    /// Return the stable scheme identifier.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        AFFINE32_SCHEME
    }

    fn update_hashed(&mut self, hash: u32) {
        let mixed = fmix32(hash);
        for ((current, multiplier), offset) in self
            .hashvalues
            .iter_mut()
            .zip(&self.multipliers)
            .zip(&self.offsets)
        {
            let permuted = multiplier.wrapping_mul(mixed).wrapping_add(*offset);
            if permuted < *current {
                *current = permuted;
            }
        }
    }

    fn ensure_compatible(&self, other: &Self) -> Result<(), MinHashError> {
        if self.seed != other.seed {
            return Err(MinHashError::IncompatibleSeed {
                left: self.seed,
                right: other.seed,
            });
        }
        if self.hashvalues.len() != other.hashvalues.len() {
            return Err(MinHashError::IncompatiblePermutationCount {
                left: self.hashvalues.len(),
                right: other.hashvalues.len(),
            });
        }
        Ok(())
    }
}

/// A 64-bit `MinHash` using affine permutations modulo `2^64`.
///
/// This is intended for extremely large distinct-element sets where a 32-bit
/// input hash can itself become the limiting collision domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinHash64 {
    seed: u64,
    hashvalues: Vec<u64>,
    multipliers: Vec<u64>,
    offsets: Vec<u64>,
}

impl MinHash64 {
    /// Create an empty sketch with `num_perm` deterministic permutations.
    pub fn new(num_perm: usize, seed: u64) -> Result<Self, MinHashError> {
        validate_num_perm(num_perm)?;

        let mut generator = SplitMix64::new(seed);
        let mut multipliers = Vec::with_capacity(num_perm);
        let mut offsets = Vec::with_capacity(num_perm);

        for _ in 0..num_perm {
            multipliers.push(generator.next_u64() | 1);
        }
        for _ in 0..num_perm {
            offsets.push(generator.next_u64());
        }

        Ok(Self {
            seed,
            hashvalues: vec![u64::MAX; num_perm],
            multipliers,
            offsets,
        })
    }

    /// Update the sketch with one byte string using Pari's default SHA-1 input
    /// hash and 64-bit affine permutation family.
    pub fn update(&mut self, value: &[u8]) {
        self.update_hashed(sha1_hash64(value));
    }

    /// Update the sketch with many values without allocating per permutation.
    pub fn update_many<I, T>(&mut self, values: I)
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        for value in values {
            self.update(value.as_ref());
        }
    }

    /// Estimate Jaccard similarity with another compatible sketch.
    pub fn jaccard(&self, other: &Self) -> Result<f64, MinHashError> {
        self.ensure_compatible(other)?;
        Ok(signature_match_ratio(&self.hashvalues, &other.hashvalues))
    }

    /// Merge another compatible sketch into this one, representing the union
    /// of the two source sets.
    pub fn merge(&mut self, other: &Self) -> Result<(), MinHashError> {
        self.ensure_compatible(other)?;
        for (current, incoming) in self.hashvalues.iter_mut().zip(&other.hashvalues) {
            *current = (*current).min(*incoming);
        }
        Ok(())
    }

    /// Reset the sketch to its empty state without rebuilding permutations.
    pub fn clear(&mut self) {
        self.hashvalues.fill(u64::MAX);
    }

    /// Return whether no values have been added since construction or reset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashvalues.iter().all(|value| *value == u64::MAX)
    }

    /// Borrow the signature values.
    #[must_use]
    pub fn signature(&self) -> &[u64] {
        &self.hashvalues
    }

    /// Return the deterministic permutation seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the number of permutation values in the signature.
    #[must_use]
    pub fn num_perm(&self) -> usize {
        self.hashvalues.len()
    }

    /// Return the stable scheme identifier.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        AFFINE64_SCHEME
    }

    fn update_hashed(&mut self, hash: u64) {
        let mixed = fmix64(hash);
        for ((current, multiplier), offset) in self
            .hashvalues
            .iter_mut()
            .zip(&self.multipliers)
            .zip(&self.offsets)
        {
            let permuted = multiplier.wrapping_mul(mixed).wrapping_add(*offset);
            if permuted < *current {
                *current = permuted;
            }
        }
    }

    fn ensure_compatible(&self, other: &Self) -> Result<(), MinHashError> {
        if self.seed != other.seed {
            return Err(MinHashError::IncompatibleSeed {
                left: self.seed,
                right: other.seed,
            });
        }
        if self.hashvalues.len() != other.hashvalues.len() {
            return Err(MinHashError::IncompatiblePermutationCount {
                left: self.hashvalues.len(),
                right: other.hashvalues.len(),
            });
        }
        Ok(())
    }
}

fn validate_num_perm(num_perm: usize) -> Result<(), MinHashError> {
    if num_perm == 0 || u32::try_from(num_perm).is_err() {
        return Err(MinHashError::InvalidPermutationCount {
            requested: num_perm,
        });
    }
    Ok(())
}

fn signature_match_ratio<T: Eq>(left: &[T], right: &[T]) -> f64 {
    debug_assert_eq!(left.len(), right.len());
    let matches = left
        .iter()
        .zip(right)
        .filter(|(left_value, right_value)| left_value == right_value)
        .count();
    let matches = u32::try_from(matches).expect("MinHash constructor limits permutation count");
    let total = u32::try_from(left.len()).expect("MinHash constructor limits permutation count");
    f64::from(matches) / f64::from(total)
}

#[derive(Debug, Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn next_u32(&mut self) -> u32 {
        let bytes = self.next_u64().to_le_bytes();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

fn fmix32(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x85EB_CA6B);
    value ^= value >> 13;
    value = value.wrapping_mul(0xC2B2_AE35);
    value ^ (value >> 16)
}

fn fmix64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

#[cfg(test)]
mod tests {
    use super::{MinHash32, MinHash64, MinHashError, AFFINE32_SCHEME, AFFINE64_SCHEME};

    #[test]
    fn rejects_zero_permutations() {
        assert_eq!(
            MinHash32::new(0, 1),
            Err(MinHashError::InvalidPermutationCount { requested: 0 })
        );
        assert_eq!(
            MinHash64::new(0, 1),
            Err(MinHashError::InvalidPermutationCount { requested: 0 })
        );
        assert_eq!(
            MinHash32::from_signature(Vec::new(), 1),
            Err(MinHashError::InvalidPermutationCount { requested: 0 })
        );
    }

    #[test]
    fn scheme_identifiers_are_explicit() {
        let minhash32 = MinHash32::new(8, 42).expect("valid sketch");
        let minhash64 = MinHash64::new(8, 42).expect("valid sketch");
        assert_eq!(minhash32.scheme(), AFFINE32_SCHEME);
        assert_eq!(minhash64.scheme(), AFFINE64_SCHEME);
    }

    #[test]
    fn precomputed_affine32_signature_round_trips_and_remains_updatable() {
        let mut original = MinHash32::new(32, 42).expect("valid sketch");
        original.update_many([&b"alpha"[..], &b"beta"[..]]);
        let signature = original.signature().to_vec();

        let mut reconstructed =
            MinHash32::from_signature(signature.clone(), 42).expect("valid signature");
        assert_eq!(reconstructed.signature(), signature);
        assert_eq!(reconstructed.seed(), original.seed());
        assert_eq!(reconstructed.num_perm(), original.num_perm());

        original.update(b"gamma");
        reconstructed.update(b"gamma");
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn affine32_seed_mapping_has_golden_signature() {
        let mut sketch = MinHash32::new(8, 42).expect("valid sketch");
        sketch.update_many([&b"a"[..], &b"b"[..], &b"c"[..]]);
        assert_eq!(
            sketch.signature(),
            &[
                555_478_325,
                368_287_343,
                517_307_743,
                440_211_845,
                1_088_094_199,
                1_274_094_295,
                904_708_659,
                327_688_530,
            ]
        );
        assert!(sketch.multipliers.iter().all(|value| value & 1 == 1));
    }

    #[test]
    fn affine64_seed_mapping_has_golden_signature() {
        let mut sketch = MinHash64::new(8, 42).expect("valid sketch");
        sketch.update_many([&b"a"[..], &b"b"[..], &b"c"[..]]);
        assert_eq!(
            sketch.signature(),
            &[
                398_824_617_996_340_472,
                4_985_036_841_737_875_763,
                430_169_245_876_064_069,
                4_830_488_362_799_227_617,
                9_007_712_658_965_612_972,
                6_350_542_169_249_656_984,
                14_245_705_141_267_314_417,
                3_755_630_306_339_185_935,
            ]
        );
        assert!(sketch.multipliers.iter().all(|value| value & 1 == 1));
    }

    #[test]
    fn update_many_matches_scalar_updates() {
        let values = [&b"alpha"[..], &b"beta"[..], &b"gamma"[..]];
        let mut batched = MinHash32::new(128, 7).expect("valid sketch");
        batched.update_many(values);

        let mut scalar = MinHash32::new(128, 7).expect("valid sketch");
        for value in values {
            scalar.update(value);
        }

        assert_eq!(batched, scalar);
    }

    #[test]
    fn merge_matches_direct_union() {
        let mut left = MinHash32::new(128, 5).expect("valid sketch");
        left.update_many([&b"a"[..], &b"b"[..]]);
        let mut right = MinHash32::new(128, 5).expect("valid sketch");
        right.update_many([&b"b"[..], &b"c"[..]]);

        left.merge(&right).expect("compatible sketches");

        let mut direct = MinHash32::new(128, 5).expect("valid sketch");
        direct.update_many([&b"a"[..], &b"b"[..], &b"c"[..]]);
        assert_eq!(left, direct);
    }

    #[test]
    fn incompatible_sketches_fail_before_similarity_or_merge() {
        let seed_one = MinHash32::new(64, 1).expect("valid sketch");
        let seed_two = MinHash32::new(64, 2).expect("valid sketch");
        assert_eq!(
            seed_one.jaccard(&seed_two),
            Err(MinHashError::IncompatibleSeed { left: 1, right: 2 })
        );

        let perm_64 = MinHash32::new(64, 1).expect("valid sketch");
        let perm_128 = MinHash32::new(128, 1).expect("valid sketch");
        assert_eq!(
            perm_64.jaccard(&perm_128),
            Err(MinHashError::IncompatiblePermutationCount {
                left: 64,
                right: 128,
            })
        );
    }

    #[test]
    fn clear_reuses_permutations_and_resets_signature() {
        let mut sketch = MinHash64::new(64, 11).expect("valid sketch");
        assert!(sketch.is_empty());
        sketch.update(b"not empty");
        assert!(!sketch.is_empty());
        sketch.clear();
        assert!(sketch.is_empty());
        assert!(sketch.signature().iter().all(|value| *value == u64::MAX));
    }

    #[test]
    fn affine32_estimate_tracks_exact_jaccard() {
        let mut left = MinHash32::new(1_024, 7).expect("valid sketch");
        let mut right = MinHash32::new(1_024, 7).expect("valid sketch");

        for value in 0_u64..1_000 {
            left.update(&value.to_le_bytes());
        }
        for value in 500_u64..1_500 {
            right.update(&value.to_le_bytes());
        }

        let estimate = left.jaccard(&right).expect("compatible sketches");
        let exact = 1.0 / 3.0;
        assert!((estimate - exact).abs() < 0.05, "estimate={estimate}");
    }

    #[test]
    fn affine64_estimate_tracks_exact_jaccard() {
        let mut left = MinHash64::new(1_024, 7).expect("valid sketch");
        let mut right = MinHash64::new(1_024, 7).expect("valid sketch");

        for value in 0_u64..1_000 {
            left.update(&value.to_le_bytes());
        }
        for value in 500_u64..1_500 {
            right.update(&value.to_le_bytes());
        }

        let estimate = left.jaccard(&right).expect("compatible sketches");
        let exact = 1.0 / 3.0;
        assert!((estimate - exact).abs() < 0.05, "estimate={estimate}");
    }
}
