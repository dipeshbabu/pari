use std::{error::Error, fmt, str::FromStr};

use super::{
    candidate_probability, simpson_integral, validate_num_perm, validate_params,
    validate_threshold, LshError, LshParams,
};

/// Version label for Pari's analytical and benchmark-calibrated planning model.
pub const LSH_PLANNER_MODEL: &str = "pari-lsh-planner-v1";

const MINHASH_VALUE_BYTES: u64 = 4;
const EXTERNAL_KEY_BYTES: u64 = 8;
const BAND_HASH_AND_MEMBERSHIP_BYTES: u64 = 16;
const IN_MEMORY_BASE_BYTES_PER_ITEM: u64 = 64;
const IN_MEMORY_BYTES_PER_BAND_ITEM: u64 = 112;
const PERSISTENT_BYTES_PER_BAND_ITEM: u64 = 48;
const PERSISTENT_FIXED_BYTES: u64 = 736;
const LAZY_RESIDENT_BYTES_PER_BAND_ITEM: u64 = 56;
const MEMORY_HEADROOM_NUMERATOR: u64 = 3;
const MEMORY_HEADROOM_DENOMINATOR: u64 = 2;

/// Storage intent supplied to the planner or recommended by its policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// Let the planner choose from the supplied capacity information.
    Auto,
    /// Keep the complete mutable index in the current process.
    Memory,
    /// Use Pari's mutable local `.pari` store.
    Persistent,
    /// Use the read-only lazy `.pari` query path.
    Lazy,
    /// Use Pari's shared Redis backend.
    Redis,
}

impl StorageMode {
    /// Stable machine-readable spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Memory => "memory",
            Self::Persistent => "persistent",
            Self::Lazy => "lazy",
            Self::Redis => "redis",
        }
    }
}

impl fmt::Display for StorageMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for StorageMode {
    type Err = LshPlanError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "memory" | "in-memory" | "in_memory" => Ok(Self::Memory),
            "persistent" | "local" => Ok(Self::Persistent),
            "lazy" | "read-only" | "read_only" => Ok(Self::Lazy),
            "redis" | "shared" => Ok(Self::Redis),
            _ => Err(LshPlanError::InvalidStorageMode {
                value: value.to_owned(),
            }),
        }
    }
}

/// Whether parameters were tuned for a new plan or read from an existing index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterSource {
    /// Parameters came from Pari's canonical optimizer.
    Tuned,
    /// Parameters came from an existing or explicitly configured index.
    Existing,
}

impl ParameterSource {
    /// Stable machine-readable spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tuned => "tuned",
            Self::Existing => "existing",
        }
    }
}

/// Explainable policy decision behind a storage recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecommendationReason {
    /// The caller explicitly selected a mode and its modeled capacity fits.
    ExplicitMode,
    /// The in-memory model, including 50% headroom, fits the supplied budget.
    InMemoryFitsBudget,
    /// In-memory does not fit, but persistent/lazy resident metadata does.
    InMemoryExceedsBudget,
    /// Neither modeled local path fits, so external storage is recommended.
    LocalMetadataExceedsBudget,
    /// No budget was supplied, so the policy chooses the safer mutable local path.
    CapacityUnknown,
    /// An explicit local mode exceeds the supplied modeled capacity.
    ExplicitModeExceedsBudget,
}

impl RecommendationReason {
    /// Stable machine-readable spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitMode => "explicit_mode",
            Self::InMemoryFitsBudget => "in_memory_fits_budget",
            Self::InMemoryExceedsBudget => "in_memory_exceeds_budget",
            Self::LocalMetadataExceedsBudget => "local_metadata_exceeds_budget",
            Self::CapacityUnknown => "capacity_unknown",
            Self::ExplicitModeExceedsBudget => "explicit_mode_exceeds_budget",
        }
    }

    /// Human explanation of the policy decision.
    #[must_use]
    pub const fn guidance(self) -> &'static str {
        match self {
            Self::ExplicitMode => {
                "The requested storage mode is retained; its modeled resident size fits the supplied budget when one was provided."
            }
            Self::InMemoryFitsBudget => {
                "The modeled in-memory index fits the supplied budget with 50% operational headroom."
            }
            Self::InMemoryExceedsBudget => {
                "The in-memory model exceeds the budget, while persistent/lazy resident metadata fits with 50% headroom."
            }
            Self::LocalMetadataExceedsBudget => {
                "Neither modeled local path fits with 50% headroom; Redis is suggested only when operating an external shared backend is acceptable."
            }
            Self::CapacityUnknown => {
                "No memory budget was supplied, so the mutable persistent store is the conservative default."
            }
            Self::ExplicitModeExceedsBudget => {
                "The requested local mode is retained, but its modeled resident size exceeds the supplied budget with 50% headroom."
            }
        }
    }
}

/// Inputs for a deterministic LSH plan or explanation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LshPlanOptions {
    /// Expected number of indexed items.
    pub expected_items: u64,
    /// Desired similarity threshold.
    pub threshold: f64,
    /// `MinHash` signature length.
    pub num_perm: usize,
    /// Optional local resident-memory budget.
    pub memory_budget_bytes: Option<u64>,
    /// Requested storage policy.
    pub storage_mode: StorageMode,
}

impl LshPlanOptions {
    /// Create options with automatic storage selection and no memory budget.
    #[must_use]
    pub const fn new(expected_items: u64, threshold: f64, num_perm: usize) -> Self {
        Self {
            expected_items,
            threshold,
            num_perm,
            memory_budget_bytes: None,
            storage_mode: StorageMode::Auto,
        }
    }

    /// Add a local resident-memory budget in bytes.
    #[must_use]
    pub const fn memory_budget_bytes(mut self, bytes: u64) -> Self {
        self.memory_budget_bytes = Some(bytes);
        self
    }

    /// Set the requested storage mode.
    #[must_use]
    pub const fn storage_mode(mut self, mode: StorageMode) -> Self {
        self.storage_mode = mode;
        self
    }
}

/// Model-based size estimates for a planned index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LshSizeEstimates {
    /// Bytes in one materialized `MinHash32` signature.
    pub signature_bytes_per_item: u64,
    /// Bytes in all materialized signatures.
    pub signature_bytes: u64,
    /// Compact key, band-hash, and membership payload bytes per item.
    pub index_metadata_bytes_per_item: u64,
    /// Compact key, band-hash, and membership payload bytes.
    pub index_metadata_bytes: u64,
    /// Calibrated in-memory index working-set bytes per item.
    pub in_memory_index_bytes_per_item: u64,
    /// Calibrated in-memory index working-set bytes.
    pub in_memory_index_bytes: u64,
    /// Modeled `.pari` bytes per item for a mostly distinct-bucket corpus.
    pub persistent_index_bytes_per_item: u64,
    /// Modeled `.pari` bytes for a mostly distinct-bucket corpus.
    pub persistent_index_bytes: u64,
    /// Calibrated resident directory/key metadata bytes per item for lazy/local reads.
    pub lazy_resident_bytes_per_item: u64,
    /// Calibrated resident directory/key metadata bytes for lazy/local reads.
    pub lazy_resident_bytes: u64,
    /// In-memory estimate with 50% operational headroom.
    pub in_memory_with_headroom_bytes: u64,
    /// Lazy/local resident estimate with 50% operational headroom.
    pub lazy_with_headroom_bytes: u64,
}

/// Deterministic LSH configuration, analytical curve, size model, and storage guidance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LshPlan {
    /// Planning model version.
    pub model: &'static str,
    /// Expected or current number of indexed items.
    pub expected_items: u64,
    /// Similarity threshold used by the configuration.
    pub threshold: f64,
    /// Signature length.
    pub num_perm: usize,
    /// Bands and rows used by the index.
    pub params: LshParams,
    /// Whether parameters were tuned or loaded from an existing index.
    pub parameter_source: ParameterSource,
    /// Number of signature values consumed by banding.
    pub used_permutations: usize,
    /// Signature values left unused by banding.
    pub unused_permutations: usize,
    /// Analytical candidate probability at `threshold`.
    pub candidate_probability_at_threshold: f64,
    /// Similarity at which the analytical candidate probability is 50%.
    pub similarity_at_50_percent_candidates: f64,
    /// Integrated candidate probability below the configured threshold.
    pub false_positive_area: f64,
    /// Integrated miss probability above the configured threshold.
    pub false_negative_area: f64,
    /// Number of bucket memberships written per item.
    pub bucket_memberships_per_item: u64,
    /// Size estimates under the versioned model.
    pub sizes: LshSizeEstimates,
    /// Optional local resident-memory budget.
    pub memory_budget_bytes: Option<u64>,
    /// Whether the in-memory estimate plus headroom fits, if a budget was supplied.
    pub in_memory_fits_budget: Option<bool>,
    /// Whether local/lazy resident metadata plus headroom fits, if supplied.
    pub persistent_fits_budget: Option<bool>,
    /// Requested storage mode.
    pub requested_storage: StorageMode,
    /// Policy recommendation.
    pub recommended_storage: StorageMode,
    /// Explainable recommendation rule.
    pub recommendation_reason: RecommendationReason,
}

impl LshPlan {
    /// Human explanation of the storage recommendation.
    #[must_use]
    pub const fn recommendation_guidance(&self) -> &'static str {
        self.recommendation_reason.guidance()
    }

    /// Analytical probability that a pair at `similarity` shares at least one band.
    #[must_use]
    pub fn candidate_probability(&self, similarity: f64) -> Option<f64> {
        if !similarity.is_finite() || !(0.0..=1.0).contains(&similarity) {
            return None;
        }
        Some(candidate_probability(
            similarity,
            self.params.bands,
            self.params.rows,
        ))
    }
}

/// Errors returned by the planner before it emits a potentially misleading estimate.
#[derive(Debug, Clone)]
pub enum LshPlanError {
    /// Expected item count must be positive.
    InvalidExpectedItems { expected_items: u64 },
    /// A supplied memory budget must be positive.
    InvalidMemoryBudget { bytes: u64 },
    /// The storage-mode spelling is unknown.
    InvalidStorageMode { value: String },
    /// An input configuration is not a valid LSH configuration.
    Lsh(LshError),
    /// A modeled byte count exceeds `u64`.
    EstimateOverflow,
}

impl fmt::Display for LshPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExpectedItems { expected_items } => write!(
                formatter,
                "expected item count must be positive, got {expected_items}"
            ),
            Self::InvalidMemoryBudget { bytes } => {
                write!(
                    formatter,
                    "memory budget must be positive, got {bytes} bytes"
                )
            }
            Self::InvalidStorageMode { value } => write!(
                formatter,
                "unknown storage mode {value:?}; expected auto, memory, persistent, lazy, or redis"
            ),
            Self::Lsh(error) => error.fmt(formatter),
            Self::EstimateOverflow => {
                formatter.write_str("planned byte estimate exceeds the supported u64 range")
            }
        }
    }
}

impl Error for LshPlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lsh(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LshError> for LshPlanError {
    fn from(error: LshError) -> Self {
        Self::Lsh(error)
    }
}

/// Plan a new index with Pari's canonical LSH parameter optimizer.
pub fn plan_lsh(options: LshPlanOptions) -> Result<LshPlan, LshPlanError> {
    if options.expected_items == 0 {
        return Err(LshPlanError::InvalidExpectedItems {
            expected_items: options.expected_items,
        });
    }
    let params = LshParams::tune(options.threshold, options.num_perm)?;
    build_plan(options, params, ParameterSource::Tuned)
}

/// Explain explicit or persisted LSH parameters without reading bucket memberships.
pub fn explain_lsh(options: LshPlanOptions, params: LshParams) -> Result<LshPlan, LshPlanError> {
    build_plan(options, params, ParameterSource::Existing)
}

fn build_plan(
    options: LshPlanOptions,
    params: LshParams,
    parameter_source: ParameterSource,
) -> Result<LshPlan, LshPlanError> {
    if options.memory_budget_bytes == Some(0) {
        return Err(LshPlanError::InvalidMemoryBudget { bytes: 0 });
    }
    validate_threshold(options.threshold)?;
    validate_num_perm(options.num_perm)?;
    validate_params(params, options.num_perm)?;

    let used_permutations = params
        .used_permutations()
        .expect("validated LSH parameters cannot overflow");
    let sizes = size_estimates(options.expected_items, options.num_perm, params.bands)?;
    let in_memory_fits_budget = options
        .memory_budget_bytes
        .map(|budget| sizes.in_memory_with_headroom_bytes <= budget);
    let persistent_fits_budget = options
        .memory_budget_bytes
        .map(|budget| sizes.lazy_with_headroom_bytes <= budget);
    let (recommended_storage, recommendation_reason) = recommend_storage(
        options.storage_mode,
        options.memory_budget_bytes,
        in_memory_fits_budget,
        persistent_fits_budget,
    );

    let false_positive_area = simpson_integral(
        |similarity| candidate_probability(similarity, params.bands, params.rows),
        0.0,
        options.threshold,
    );
    let false_negative_area = simpson_integral(
        |similarity| 1.0 - candidate_probability(similarity, params.bands, params.rows),
        options.threshold,
        1.0,
    );
    let bands = f64::from(u32::try_from(params.bands).expect("validated bands fit u32"));
    let rows = f64::from(u32::try_from(params.rows).expect("validated rows fit u32"));
    let similarity_at_50_percent_candidates = (1.0 - 0.5_f64.powf(1.0 / bands)).powf(1.0 / rows);

    Ok(LshPlan {
        model: LSH_PLANNER_MODEL,
        expected_items: options.expected_items,
        threshold: options.threshold,
        num_perm: options.num_perm,
        params,
        parameter_source,
        used_permutations,
        unused_permutations: options.num_perm - used_permutations,
        candidate_probability_at_threshold: candidate_probability(
            options.threshold,
            params.bands,
            params.rows,
        ),
        similarity_at_50_percent_candidates,
        false_positive_area,
        false_negative_area,
        bucket_memberships_per_item: u64::try_from(params.bands)
            .map_err(|_| LshPlanError::EstimateOverflow)?,
        sizes,
        memory_budget_bytes: options.memory_budget_bytes,
        in_memory_fits_budget,
        persistent_fits_budget,
        requested_storage: options.storage_mode,
        recommended_storage,
        recommendation_reason,
    })
}

fn size_estimates(
    expected_items: u64,
    num_perm: usize,
    bands: usize,
) -> Result<LshSizeEstimates, LshPlanError> {
    let num_perm = u64::try_from(num_perm).map_err(|_| LshPlanError::EstimateOverflow)?;
    let bands = u64::try_from(bands).map_err(|_| LshPlanError::EstimateOverflow)?;
    let signature_bytes_per_item = checked_mul(num_perm, MINHASH_VALUE_BYTES)?;
    let index_metadata_bytes_per_item = checked_add(
        EXTERNAL_KEY_BYTES,
        checked_mul(bands, BAND_HASH_AND_MEMBERSHIP_BYTES)?,
    )?;
    let in_memory_index_bytes_per_item = checked_add(
        IN_MEMORY_BASE_BYTES_PER_ITEM,
        checked_mul(bands, IN_MEMORY_BYTES_PER_BAND_ITEM)?,
    )?;
    let persistent_index_bytes_per_item = checked_add(
        EXTERNAL_KEY_BYTES,
        checked_mul(bands, PERSISTENT_BYTES_PER_BAND_ITEM)?,
    )?;
    let lazy_resident_bytes_per_item = checked_add(
        EXTERNAL_KEY_BYTES,
        checked_mul(bands, LAZY_RESIDENT_BYTES_PER_BAND_ITEM)?,
    )?;
    let in_memory_index_bytes = checked_mul(expected_items, in_memory_index_bytes_per_item)?;
    let lazy_resident_bytes = checked_mul(expected_items, lazy_resident_bytes_per_item)?;

    Ok(LshSizeEstimates {
        signature_bytes_per_item,
        signature_bytes: checked_mul(expected_items, signature_bytes_per_item)?,
        index_metadata_bytes_per_item,
        index_metadata_bytes: checked_mul(expected_items, index_metadata_bytes_per_item)?,
        in_memory_index_bytes_per_item,
        in_memory_index_bytes,
        persistent_index_bytes_per_item,
        persistent_index_bytes: checked_add(
            PERSISTENT_FIXED_BYTES,
            checked_mul(expected_items, persistent_index_bytes_per_item)?,
        )?,
        lazy_resident_bytes_per_item,
        lazy_resident_bytes,
        in_memory_with_headroom_bytes: with_headroom(in_memory_index_bytes)?,
        lazy_with_headroom_bytes: with_headroom(lazy_resident_bytes)?,
    })
}

fn recommend_storage(
    requested: StorageMode,
    budget: Option<u64>,
    in_memory_fits: Option<bool>,
    persistent_fits: Option<bool>,
) -> (StorageMode, RecommendationReason) {
    match requested {
        StorageMode::Auto => match budget {
            None => (
                StorageMode::Persistent,
                RecommendationReason::CapacityUnknown,
            ),
            Some(_) if in_memory_fits == Some(true) => (
                StorageMode::Memory,
                RecommendationReason::InMemoryFitsBudget,
            ),
            Some(_) if persistent_fits == Some(true) => (
                StorageMode::Persistent,
                RecommendationReason::InMemoryExceedsBudget,
            ),
            Some(_) => (
                StorageMode::Redis,
                RecommendationReason::LocalMetadataExceedsBudget,
            ),
        },
        StorageMode::Memory => match budget {
            None => (StorageMode::Memory, RecommendationReason::ExplicitMode),
            Some(_) if in_memory_fits == Some(true) => (
                StorageMode::Memory,
                RecommendationReason::InMemoryFitsBudget,
            ),
            Some(_) if persistent_fits == Some(true) => (
                StorageMode::Persistent,
                RecommendationReason::InMemoryExceedsBudget,
            ),
            Some(_) => (
                StorageMode::Redis,
                RecommendationReason::LocalMetadataExceedsBudget,
            ),
        },
        StorageMode::Persistent | StorageMode::Lazy => {
            let reason = if budget.is_some() && persistent_fits == Some(false) {
                RecommendationReason::ExplicitModeExceedsBudget
            } else {
                RecommendationReason::ExplicitMode
            };
            (requested, reason)
        }
        StorageMode::Redis => (StorageMode::Redis, RecommendationReason::ExplicitMode),
    }
}

fn with_headroom(bytes: u64) -> Result<u64, LshPlanError> {
    let scaled = checked_mul(bytes, MEMORY_HEADROOM_NUMERATOR)?;
    Ok(scaled.div_ceil(MEMORY_HEADROOM_DENOMINATOR))
}

fn checked_mul(left: u64, right: u64) -> Result<u64, LshPlanError> {
    left.checked_mul(right)
        .ok_or(LshPlanError::EstimateOverflow)
}

fn checked_add(left: u64, right: u64) -> Result<u64, LshPlanError> {
    left.checked_add(right)
        .ok_or(LshPlanError::EstimateOverflow)
}

#[cfg(test)]
mod tests {
    use pari_core::MinHash32;

    use super::{
        explain_lsh, plan_lsh, LshPlanError, LshPlanOptions, ParameterSource, RecommendationReason,
        StorageMode,
    };
    use crate::{LshIndex32, LshParams};

    #[test]
    fn planner_reuses_canonical_tuning_and_has_stable_estimates() {
        let plan = plan_lsh(LshPlanOptions::new(1_000_000, 0.8, 128)).expect("plan");
        let index = LshIndex32::new(0.8, 128, 7).expect("index");
        assert_eq!(plan.params, index.params());
        assert_eq!(plan.params, LshParams::new(9, 13));
        assert_eq!(plan.parameter_source, ParameterSource::Tuned);
        assert_eq!(plan.used_permutations, 117);
        assert_eq!(plan.unused_permutations, 11);
        assert_eq!(plan.bucket_memberships_per_item, 9);
        assert_eq!(plan.sizes.signature_bytes_per_item, 512);
        assert_eq!(plan.sizes.index_metadata_bytes_per_item, 152);
        assert_eq!(plan.sizes.in_memory_index_bytes_per_item, 1_072);
        assert_eq!(plan.sizes.persistent_index_bytes_per_item, 440);
        assert_eq!(plan.sizes.persistent_index_bytes, 440_000_736);
        assert_eq!(plan.sizes.lazy_resident_bytes_per_item, 512);
        assert_eq!(plan.recommended_storage, StorageMode::Persistent);
        assert_eq!(
            plan.recommendation_reason,
            RecommendationReason::CapacityUnknown
        );
        assert_eq!(plan.candidate_probability(-0.1), None);
        assert_eq!(plan.candidate_probability(1.1), None);
    }

    #[test]
    fn budget_policy_is_explicit_and_deterministic() {
        let base = LshPlanOptions::new(1_000, 0.8, 128);
        let memory = plan_lsh(base.memory_budget_bytes(2_000_000)).expect("memory plan");
        assert_eq!(memory.recommended_storage, StorageMode::Memory);
        assert_eq!(memory.in_memory_fits_budget, Some(true));

        let persistent = plan_lsh(base.memory_budget_bytes(1_000_000)).expect("persistent plan");
        assert_eq!(persistent.recommended_storage, StorageMode::Persistent);
        assert_eq!(
            persistent.recommendation_reason,
            RecommendationReason::InMemoryExceedsBudget
        );

        let redis = plan_lsh(base.memory_budget_bytes(100_000)).expect("redis plan");
        assert_eq!(redis.recommended_storage, StorageMode::Redis);
        assert_eq!(redis.persistent_fits_budget, Some(false));

        let explicit = plan_lsh(
            base.memory_budget_bytes(100_000)
                .storage_mode(StorageMode::Lazy),
        )
        .expect("explicit plan");
        assert_eq!(explicit.recommended_storage, StorageMode::Lazy);
        assert_eq!(
            explicit.recommendation_reason,
            RecommendationReason::ExplicitModeExceedsBudget
        );
    }

    #[test]
    fn existing_explanation_preserves_explicit_params() {
        let plan = explain_lsh(
            LshPlanOptions::new(10, 0.8, 128).storage_mode(StorageMode::Persistent),
            LshParams::new(16, 8),
        )
        .expect("explanation");
        assert_eq!(plan.parameter_source, ParameterSource::Existing);
        assert_eq!(plan.params, LshParams::new(16, 8));
        assert_eq!(plan.unused_permutations, 0);
        assert_eq!(plan.recommended_storage, StorageMode::Persistent);

        let empty = explain_lsh(
            LshPlanOptions::new(0, 0.8, 128).storage_mode(StorageMode::Persistent),
            LshParams::new(16, 8),
        )
        .expect("empty explanation");
        assert_eq!(empty.expected_items, 0);
        assert_eq!(empty.sizes.persistent_index_bytes, 736);
    }

    #[test]
    fn invalid_and_overflowing_requests_fail_explicitly() {
        assert!(matches!(
            plan_lsh(LshPlanOptions::new(0, 0.8, 128)),
            Err(LshPlanError::InvalidExpectedItems { .. })
        ));
        assert!(matches!(
            plan_lsh(LshPlanOptions::new(1, 0.8, 128).memory_budget_bytes(0)),
            Err(LshPlanError::InvalidMemoryBudget { .. })
        ));
        assert!(matches!(
            plan_lsh(LshPlanOptions::new(u64::MAX, 0.8, 128)),
            Err(LshPlanError::EstimateOverflow)
        ));
    }

    #[test]
    fn analytical_candidate_curve_matches_controlled_lsh_workloads() {
        let plan = plan_lsh(LshPlanOptions::new(2_000, 0.8, 128)).expect("plan");
        for (ordinal, similarity) in [0.5, 0.8, 0.9].into_iter().enumerate() {
            let observed = observed_candidate_rate(plan.params, similarity, 2_000, ordinal as u64);
            let expected = plan
                .candidate_probability(similarity)
                .expect("valid similarity");
            assert!(
                (observed - expected).abs() <= 0.04,
                "similarity={similarity}, expected={expected}, observed={observed}"
            );
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn capacity_model_stays_within_documented_campaign_tolerances() {
        let plan = plan_lsh(LshPlanOptions::new(1_000_000, 0.8, 128)).expect("plan");
        let persistent = plan.sizes.persistent_index_bytes_per_item as f64;
        for measured in [440.002_32, 440.000_736] {
            assert!(relative_error(persistent, measured) <= 0.05);
        }

        let lazy = plan.sizes.lazy_resident_bytes_per_item as f64;
        for measured in [519.495_68, 504.582_144] {
            assert!(relative_error(lazy, measured) <= 0.15);
        }

        let memory = plan.sizes.in_memory_index_bytes_per_item as f64;
        for measured in [824.360_96, 1_067.565_056] {
            assert!(relative_error(memory, measured) <= 0.35);
        }
    }

    fn relative_error(modeled: f64, measured: f64) -> f64 {
        (modeled - measured).abs() / measured
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn observed_candidate_rate(
        params: LshParams,
        similarity: f64,
        trials: usize,
        salt: u64,
    ) -> f64 {
        let num_perm = 128;
        let seed = 7;
        let mut state = 0xA076_1D64_78BD_642F_u64 ^ salt;
        let mut references = Vec::with_capacity(trials);
        let mut queries = Vec::with_capacity(trials);
        for _ in 0..trials {
            let mut reference = Vec::with_capacity(num_perm);
            let mut query = Vec::with_capacity(num_perm);
            for _ in 0..num_perm {
                let value = next_u64(&mut state) as u32;
                reference.push(value);
                let sample = next_u64(&mut state) as f64 / u64::MAX as f64;
                if sample < similarity {
                    query.push(value);
                } else {
                    query.push(value ^ ((next_u64(&mut state) as u32) | 1));
                }
            }
            references.push(MinHash32::from_signature(reference, seed).expect("reference"));
            queries.push(MinHash32::from_signature(query, seed).expect("query"));
        }

        let mut index = LshIndex32::with_params(0.8, num_perm, seed, params).expect("index");
        index
            .insert_many(
                references
                    .iter()
                    .enumerate()
                    .map(|(key, sketch)| (key as u64, sketch)),
            )
            .expect("insert");
        let candidates = index.query_many(&queries).expect("queries");
        let matches = candidates
            .iter()
            .enumerate()
            .filter(|(key, candidates)| candidates.binary_search(&(*key as u64)).is_ok())
            .count();
        matches as f64 / trials as f64
    }

    fn next_u64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = *state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}
