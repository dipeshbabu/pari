#![forbid(unsafe_code)]
//! Batch-first locality-sensitive hashing indexes for Pari.
//!
//! The first index targets [`pari_core::MinHash32`]. It returns approximate
//! candidates rather than pretending LSH band collisions are exact similarity
//! verification. Application-specific verification can be layered on later.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
};

use pari_core::MinHash32;

const AUTO_TUNE_INTEGRATION_SEGMENTS: u32 = 64;
const MAX_AUTO_TUNE_PERMUTATIONS: usize = 4_096;
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// The number of LSH bands and rows used from each `MinHash` signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LshParams {
    /// Number of independent hash tables.
    pub bands: usize,
    /// Number of consecutive signature values in each band.
    pub rows: usize,
}

impl LshParams {
    /// Construct explicit LSH parameters.
    #[must_use]
    pub const fn new(bands: usize, rows: usize) -> Self {
        Self { bands, rows }
    }

    /// Return the number of signature values consumed by the index.
    #[must_use]
    pub const fn used_permutations(self) -> Option<usize> {
        self.bands.checked_mul(self.rows)
    }
}

/// Errors returned by the in-memory LSH index.
#[derive(Debug, Clone)]
pub enum LshError {
    /// The requested similarity threshold is not finite or is outside `(0, 1]`.
    InvalidThreshold { threshold: f64 },
    /// The requested signature length is not representable by Pari's current
    /// compatibility metadata.
    InvalidPermutationCount { requested: usize },
    /// Automatic parameter tuning is intentionally bounded; callers with very
    /// large signatures should supply explicit [`LshParams`].
    AutomaticTuningTooLarge { requested: usize, max: usize },
    /// Bands and rows are zero, overflow, or consume more values than the
    /// configured signature length.
    InvalidParams {
        bands: usize,
        rows: usize,
        num_perm: usize,
    },
    /// A supplied sketch was built from a different permutation seed.
    IncompatibleSeed { expected: u64, actual: u64 },
    /// A supplied sketch has a different signature length.
    IncompatiblePermutationCount { expected: usize, actual: usize },
    /// The external key already exists in the index or appears twice in one
    /// insertion batch.
    DuplicateKey { key: u64 },
    /// The append-only internal identifier space would be exhausted.
    TooManyItems,
}

impl fmt::Display for LshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidThreshold { threshold } => write!(
                formatter,
                "threshold must be finite and in (0, 1], got {threshold}"
            ),
            Self::InvalidPermutationCount { requested } => write!(
                formatter,
                "num_perm must be in 1..={}, got {requested}",
                u32::MAX
            ),
            Self::AutomaticTuningTooLarge { requested, max } => write!(
                formatter,
                "automatic LSH tuning supports at most {max} permutations, got {requested}; use explicit LshParams"
            ),
            Self::InvalidParams {
                bands,
                rows,
                num_perm,
            } => write!(
                formatter,
                "invalid LSH parameters: bands={bands}, rows={rows}, num_perm={num_perm}"
            ),
            Self::IncompatibleSeed { expected, actual } => write!(
                formatter,
                "incompatible MinHash seed: expected {expected}, got {actual}"
            ),
            Self::IncompatiblePermutationCount { expected, actual } => write!(
                formatter,
                "incompatible MinHash permutation count: expected {expected}, got {actual}"
            ),
            Self::DuplicateKey { key } => write!(formatter, "key {key} already exists in the index"),
            Self::TooManyItems => formatter.write_str("internal LSH item identifier space exhausted"),
        }
    }
}

impl Error for LshError {}

/// An in-memory threshold LSH index for [`MinHash32`].
///
/// The index stores compact `u32` identifiers in hash buckets and keeps user
/// keys outside the hot bucket path. Query results are LSH candidates, sorted
/// by external key for deterministic output; they are not exact Jaccard
/// verification results.
#[derive(Debug)]
pub struct LshIndex32 {
    threshold: f64,
    num_perm: usize,
    seed: u64,
    params: LshParams,
    buckets: Vec<HashMap<u64, Vec<u32>>>,
    key_to_id: HashMap<u64, u32>,
    id_to_key: Vec<Option<u64>>,
    band_hashes: Vec<Option<Vec<u64>>>,
}

impl LshIndex32 {
    /// Create an index and automatically choose bands and rows for `threshold`.
    ///
    /// Automatic tuning numerically minimizes equal-weight false-positive and
    /// false-negative probability area, following the same objective used by
    /// datasketch's `MinHash` LSH optimizer without requiring `SciPy` at runtime.
    pub fn new(threshold: f64, num_perm: usize, seed: u64) -> Result<Self, LshError> {
        validate_threshold(threshold)?;
        validate_num_perm(num_perm)?;
        if num_perm > MAX_AUTO_TUNE_PERMUTATIONS {
            return Err(LshError::AutomaticTuningTooLarge {
                requested: num_perm,
                max: MAX_AUTO_TUNE_PERMUTATIONS,
            });
        }
        let params = optimize_params(threshold, num_perm);
        Self::with_params(threshold, num_perm, seed, params)
    }

    /// Create an index with explicit banding parameters.
    pub fn with_params(
        threshold: f64,
        num_perm: usize,
        seed: u64,
        params: LshParams,
    ) -> Result<Self, LshError> {
        validate_threshold(threshold)?;
        validate_num_perm(num_perm)?;
        validate_params(params, num_perm)?;

        let buckets = std::iter::repeat_with(HashMap::new)
            .take(params.bands)
            .collect();

        Ok(Self {
            threshold,
            num_perm,
            seed,
            params,
            buckets,
            key_to_id: HashMap::new(),
            id_to_key: Vec::new(),
            band_hashes: Vec::new(),
        })
    }

    /// Insert one key and signature.
    pub fn insert(&mut self, key: u64, sketch: &MinHash32) -> Result<(), LshError> {
        self.insert_many(std::iter::once((key, sketch)))
    }

    /// Insert a batch atomically with respect to validation failures.
    ///
    /// The entire batch is checked for duplicate keys, incompatible signatures,
    /// and internal capacity before the first bucket is mutated.
    pub fn insert_many<'a>(
        &mut self,
        items: impl IntoIterator<Item = (u64, &'a MinHash32)>,
    ) -> Result<(), LshError> {
        let items: Vec<_> = items.into_iter().collect();
        let mut batch_keys = HashSet::with_capacity(items.len());

        for (key, sketch) in &items {
            if self.key_to_id.contains_key(key) || !batch_keys.insert(*key) {
                return Err(LshError::DuplicateKey { key: *key });
            }
            self.ensure_compatible(sketch)?;
        }
        self.ensure_capacity(items.len())?;

        for (key, sketch) in items {
            self.insert_validated(key, sketch);
        }
        Ok(())
    }

    /// Query approximate candidates for one signature.
    pub fn query(&self, sketch: &MinHash32) -> Result<Vec<u64>, LshError> {
        self.ensure_compatible(sketch)?;
        let mut candidates = HashSet::new();
        self.collect_candidate_ids(sketch.signature(), &mut candidates);
        Ok(self.keys_for_candidates(&candidates))
    }

    /// Query many signatures while reusing the candidate scratch set.
    pub fn query_many<'a>(
        &self,
        sketches: impl IntoIterator<Item = &'a MinHash32>,
    ) -> Result<Vec<Vec<u64>>, LshError> {
        let mut output = Vec::new();
        let mut candidates = HashSet::new();

        for sketch in sketches {
            self.ensure_compatible(sketch)?;
            candidates.clear();
            self.collect_candidate_ids(sketch.signature(), &mut candidates);
            output.push(self.keys_for_candidates(&candidates));
        }
        Ok(output)
    }

    /// Remove a key if it exists, returning whether anything changed.
    pub fn remove(&mut self, key: u64) -> bool {
        let Some(&id) = self.key_to_id.get(&key) else {
            return false;
        };
        let Ok(index) = usize::try_from(id) else {
            return false;
        };
        if self.id_to_key.get(index).and_then(|slot| *slot) != Some(key) {
            return false;
        }
        let Some(hashes) = self.band_hashes.get_mut(index).and_then(Option::take) else {
            return false;
        };

        self.key_to_id.remove(&key);
        for (table, hash) in self.buckets.iter_mut().zip(hashes) {
            let remove_bucket = if let Some(ids) = table.get_mut(&hash) {
                ids.retain(|candidate| *candidate != id);
                ids.is_empty()
            } else {
                false
            };
            if remove_bucket {
                table.remove(&hash);
            }
        }
        if let Some(slot) = self.id_to_key.get_mut(index) {
            *slot = None;
        }
        true
    }

    /// Return whether an external key is currently indexed.
    #[must_use]
    pub fn contains(&self, key: u64) -> bool {
        self.key_to_id.contains_key(&key)
    }

    /// Return the number of live keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.key_to_id.len()
    }

    /// Return whether the index contains no live keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key_to_id.is_empty()
    }

    /// Return the configured target threshold.
    #[must_use]
    pub const fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Return the configured signature length.
    #[must_use]
    pub const fn num_perm(&self) -> usize {
        self.num_perm
    }

    /// Return the required `MinHash` seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Return the selected banding parameters.
    #[must_use]
    pub const fn params(&self) -> LshParams {
        self.params
    }

    fn ensure_compatible(&self, sketch: &MinHash32) -> Result<(), LshError> {
        if sketch.seed() != self.seed {
            return Err(LshError::IncompatibleSeed {
                expected: self.seed,
                actual: sketch.seed(),
            });
        }
        if sketch.num_perm() != self.num_perm {
            return Err(LshError::IncompatiblePermutationCount {
                expected: self.num_perm,
                actual: sketch.num_perm(),
            });
        }
        Ok(())
    }

    fn ensure_capacity(&self, additional: usize) -> Result<(), LshError> {
        let projected = self
            .id_to_key
            .len()
            .checked_add(additional)
            .ok_or(LshError::TooManyItems)?;
        let max_items = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
        if projected > max_items {
            return Err(LshError::TooManyItems);
        }
        Ok(())
    }

    fn insert_validated(&mut self, key: u64, sketch: &MinHash32) {
        let id = u32::try_from(self.id_to_key.len()).expect("capacity validated before insertion");
        let hashes = self.compute_band_hashes(sketch.signature());

        for (table, hash) in self.buckets.iter_mut().zip(&hashes) {
            table.entry(*hash).or_default().push(id);
        }

        self.key_to_id.insert(key, id);
        self.id_to_key.push(Some(key));
        self.band_hashes.push(Some(hashes));
    }

    fn compute_band_hashes(&self, signature: &[u32]) -> Vec<u64> {
        let used = self
            .params
            .used_permutations()
            .expect("validated LSH parameters cannot overflow");
        signature[..used]
            .chunks_exact(self.params.rows)
            .map(hash_band)
            .collect()
    }

    fn collect_candidate_ids(&self, signature: &[u32], output: &mut HashSet<u32>) {
        let hashes = self.compute_band_hashes(signature);
        for (table, hash) in self.buckets.iter().zip(hashes) {
            if let Some(ids) = table.get(&hash) {
                output.extend(ids.iter().copied());
            }
        }
    }

    fn keys_for_candidates(&self, candidates: &HashSet<u32>) -> Vec<u64> {
        let mut keys = Vec::with_capacity(candidates.len());
        for id in candidates {
            let index =
                usize::try_from(*id).expect("u32 identifier fits usize on supported targets");
            if let Some(Some(key)) = self.id_to_key.get(index) {
                keys.push(*key);
            }
        }
        keys.sort_unstable();
        keys
    }
}

fn validate_threshold(threshold: f64) -> Result<(), LshError> {
    if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
        return Err(LshError::InvalidThreshold { threshold });
    }
    Ok(())
}

fn validate_num_perm(num_perm: usize) -> Result<(), LshError> {
    if num_perm == 0 || u32::try_from(num_perm).is_err() {
        return Err(LshError::InvalidPermutationCount {
            requested: num_perm,
        });
    }
    Ok(())
}

fn validate_params(params: LshParams, num_perm: usize) -> Result<(), LshError> {
    let Some(used) = params.used_permutations() else {
        return Err(LshError::InvalidParams {
            bands: params.bands,
            rows: params.rows,
            num_perm,
        });
    };
    if params.bands == 0 || params.rows == 0 || used > num_perm {
        return Err(LshError::InvalidParams {
            bands: params.bands,
            rows: params.rows,
            num_perm,
        });
    }
    Ok(())
}

fn optimize_params(threshold: f64, num_perm: usize) -> LshParams {
    let mut best_params = LshParams::new(1, 1);
    let mut best_error = f64::INFINITY;
    let mut best_used = 1;

    for bands in 1..=num_perm {
        for rows in 1..=(num_perm / bands) {
            let false_positive = simpson_integral(
                |similarity| candidate_probability(similarity, bands, rows),
                0.0,
                threshold,
            );
            let false_negative = simpson_integral(
                |similarity| 1.0 - candidate_probability(similarity, bands, rows),
                threshold,
                1.0,
            );
            let error = 0.5 * (false_positive + false_negative);
            let used = bands * rows;
            let better = match error.total_cmp(&best_error) {
                Ordering::Less => true,
                Ordering::Equal => used > best_used,
                Ordering::Greater => false,
            };
            if better {
                best_error = error;
                best_used = used;
                best_params = LshParams::new(bands, rows);
            }
        }
    }
    best_params
}

fn candidate_probability(similarity: f64, bands: usize, rows: usize) -> f64 {
    let bands = u32::try_from(bands).expect("validated permutation count bounds bands");
    let rows = u32::try_from(rows).expect("validated permutation count bounds rows");
    1.0 - (1.0 - similarity.powf(f64::from(rows))).powf(f64::from(bands))
}

fn simpson_integral(function: impl Fn(f64) -> f64, start: f64, end: f64) -> f64 {
    if start.total_cmp(&end) == Ordering::Equal {
        return 0.0;
    }
    debug_assert_eq!(AUTO_TUNE_INTEGRATION_SEGMENTS % 2, 0);
    let step = (end - start) / f64::from(AUTO_TUNE_INTEGRATION_SEGMENTS);
    let mut total = function(start) + function(end);
    for segment in 1..AUTO_TUNE_INTEGRATION_SEGMENTS {
        let x = start + f64::from(segment) * step;
        let weight = if segment % 2 == 0 { 2.0 } else { 4.0 };
        total += weight * function(x);
    }
    total * step / 3.0
}

fn hash_band(values: &[u32]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for value in values {
        hash ^= u64::from(*value);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::try_from(values.len()).expect("band length is bounded by num_perm");
    avalanche64(hash)
}

fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use pari_core::MinHash32;

    use super::{LshError, LshIndex32, LshParams};

    fn sketch(values: impl IntoIterator<Item = u64>, num_perm: usize, seed: u64) -> MinHash32 {
        let mut sketch = MinHash32::new(num_perm, seed).expect("valid test sketch");
        for value in values {
            sketch.update(&value.to_le_bytes());
        }
        sketch
    }

    #[test]
    fn automatic_tuning_is_stable() {
        let index = LshIndex32::new(0.8, 128, 7).expect("valid index");
        assert_eq!(index.params(), LshParams::new(9, 13));
        assert_eq!(index.params().used_permutations(), Some(117));
    }

    #[test]
    fn invalid_threshold_and_params_fail_cleanly() {
        assert!(matches!(
            LshIndex32::new(f64::NAN, 128, 1),
            Err(LshError::InvalidThreshold { .. })
        ));
        assert!(matches!(
            LshIndex32::with_params(0.8, 128, 1, LshParams::new(33, 4)),
            Err(LshError::InvalidParams { .. })
        ));
    }

    #[test]
    fn query_finds_near_candidate_and_rejects_distant_item() {
        let query = sketch(0..100, 128, 7);
        let similar = sketch((0..90).chain(100..110), 128, 7);
        let distant = sketch(1_000..1_100, 128, 7);
        let mut index =
            LshIndex32::with_params(0.8, 128, 7, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(20, &similar), (30, &distant)])
            .expect("valid batch");

        assert_eq!(index.query(&query).expect("compatible query"), vec![20]);
    }

    #[test]
    fn remove_cleans_membership_and_buckets() {
        let signature = sketch(0..100, 128, 3);
        let mut index =
            LshIndex32::with_params(0.8, 128, 3, LshParams::new(32, 4)).expect("valid index");
        index.insert(7, &signature).expect("valid insert");
        assert!(index.contains(7));
        assert_eq!(index.query(&signature).expect("compatible query"), vec![7]);

        assert!(index.remove(7));
        assert!(!index.contains(7));
        assert!(index
            .query(&signature)
            .expect("compatible query")
            .is_empty());
        assert!(!index.remove(7));
        assert!(index.is_empty());
    }

    #[test]
    fn scalar_and_batch_paths_are_equivalent() {
        let first = sketch(0..100, 128, 9);
        let second = sketch(50..150, 128, 9);
        let third = sketch(1_000..1_100, 128, 9);
        let params = LshParams::new(32, 4);
        let mut scalar = LshIndex32::with_params(0.8, 128, 9, params).expect("valid index");
        let mut batch = LshIndex32::with_params(0.8, 128, 9, params).expect("valid index");

        scalar.insert(1, &first).expect("valid insert");
        scalar.insert(2, &second).expect("valid insert");
        scalar.insert(3, &third).expect("valid insert");
        batch
            .insert_many([(1, &first), (2, &second), (3, &third)])
            .expect("valid batch");

        let queries = [&first, &second, &third];
        assert_eq!(
            scalar.query_many(queries).expect("compatible queries"),
            batch.query_many(queries).expect("compatible queries")
        );
    }

    #[test]
    fn batch_validation_is_atomic() {
        let first = sketch(0..100, 128, 5);
        let second = sketch(50..150, 128, 5);
        let mut index =
            LshIndex32::with_params(0.8, 128, 5, LshParams::new(32, 4)).expect("valid index");
        index.insert(1, &first).expect("valid insert");

        assert!(matches!(
            index.insert_many([(2, &second), (1, &first)]),
            Err(LshError::DuplicateKey { key: 1 })
        ));
        assert_eq!(index.len(), 1);
        assert!(!index.contains(2));

        let mut empty =
            LshIndex32::with_params(0.8, 128, 5, LshParams::new(32, 4)).expect("valid index");
        assert!(matches!(
            empty.insert_many([(2, &first), (2, &second)]),
            Err(LshError::DuplicateKey { key: 2 })
        ));
        assert!(empty.is_empty());
    }

    #[test]
    fn incompatible_signatures_fail_before_mutation_or_query() {
        let expected = sketch(0..100, 128, 5);
        let wrong_seed = sketch(0..100, 128, 6);
        let wrong_length = sketch(0..100, 64, 5);
        let mut index =
            LshIndex32::with_params(0.8, 128, 5, LshParams::new(32, 4)).expect("valid index");
        index.insert(1, &expected).expect("valid insert");

        assert!(matches!(
            index.query(&wrong_seed),
            Err(LshError::IncompatibleSeed {
                expected: 5,
                actual: 6
            })
        ));
        assert!(matches!(
            index.insert(2, &wrong_length),
            Err(LshError::IncompatiblePermutationCount {
                expected: 128,
                actual: 64
            })
        ));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn query_output_is_sorted_by_external_key() {
        let signature = sketch(0..100, 128, 4);
        let mut index =
            LshIndex32::with_params(0.8, 128, 4, LshParams::new(32, 4)).expect("valid index");
        index
            .insert_many([(9, &signature), (1, &signature), (5, &signature)])
            .expect("valid batch");
        assert_eq!(
            index.query(&signature).expect("compatible query"),
            vec![1, 5, 9]
        );
    }

    #[test]
    fn immutable_queries_can_run_concurrently() {
        let signature = sketch(0..100, 128, 12);
        let mut index =
            LshIndex32::with_params(0.8, 128, 12, LshParams::new(32, 4)).expect("valid index");
        index.insert(42, &signature).expect("valid insert");

        thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let index_ref = &index;
                let query_ref = &signature;
                handles.push(scope.spawn(move || index_ref.query(query_ref).expect("valid query")));
            }
            for handle in handles {
                assert_eq!(
                    handle.join().expect("query thread should not panic"),
                    vec![42]
                );
            }
        });
    }
}
