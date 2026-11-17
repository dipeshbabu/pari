//! Similarity scores for LSH candidates.
//!
//! Indexes that keep full signatures can rank their LSH candidates by the
//! fraction of equal signature values, the standard `MinHash` Jaccard estimate.

use std::{error::Error, fmt};

/// One LSH candidate with its estimated Jaccard similarity to the query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredCandidate {
    /// External key of the candidate.
    pub key: u64,
    /// Fraction of equal signature values, an unbiased Jaccard estimate.
    pub similarity: f64,
}

/// A similarity filter that is out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreFilterError {
    reason: &'static str,
}

impl ScoreFilterError {
    /// Return the reason the filter was rejected.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for ScoreFilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid similarity filter: {}", self.reason)
    }
}

impl Error for ScoreFilterError {}

/// Filters applied to similarity-scored query results.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScoreFilter {
    min_similarity: Option<f64>,
    top_k: Option<usize>,
}

impl ScoreFilter {
    /// Return every candidate, most similar first.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            min_similarity: None,
            top_k: None,
        }
    }

    /// Keep candidates whose estimated similarity is at least `threshold`.
    #[must_use]
    pub const fn min_similarity(mut self, threshold: f64) -> Self {
        self.min_similarity = Some(threshold);
        self
    }

    /// Keep at most `count` candidates after sorting by similarity.
    #[must_use]
    pub const fn top_k(mut self, count: usize) -> Self {
        self.top_k = Some(count);
        self
    }

    /// Reject a non-finite or out-of-range threshold and a zero `top_k`.
    pub fn validate(self) -> Result<(), ScoreFilterError> {
        if let Some(threshold) = self.min_similarity {
            if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
                return Err(ScoreFilterError {
                    reason: "min_similarity must be finite and in [0, 1]",
                });
            }
        }
        if self.top_k == Some(0) {
            return Err(ScoreFilterError {
                reason: "top_k must be positive",
            });
        }
        Ok(())
    }

    /// Filter, sort (most similar first, ties by key), and truncate candidates.
    #[must_use]
    pub fn apply(self, mut scored: Vec<ScoredCandidate>) -> Vec<ScoredCandidate> {
        if let Some(threshold) = self.min_similarity {
            scored.retain(|candidate| candidate.similarity >= threshold);
        }
        scored.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then(left.key.cmp(&right.key))
        });
        if let Some(count) = self.top_k {
            scored.truncate(count);
        }
        scored
    }
}

/// Fraction of positions where two equal-length signatures agree.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn signature_similarity(left: &[u64], right: &[u64]) -> f64 {
    if left.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    let matches = left
        .iter()
        .zip(right)
        .filter(|(left, right)| left == right)
        .count();
    matches as f64 / left.len() as f64
}

#[cfg(test)]
mod tests {
    use super::{signature_similarity, ScoreFilter, ScoredCandidate};

    fn candidate(key: u64, similarity: f64) -> ScoredCandidate {
        ScoredCandidate { key, similarity }
    }

    #[test]
    fn filters_sort_and_truncate() {
        let scored = vec![
            candidate(3, 0.5),
            candidate(1, 0.9),
            candidate(2, 0.9),
            candidate(4, 0.1),
        ];
        assert_eq!(
            ScoreFilter::new().apply(scored.clone()),
            vec![
                candidate(1, 0.9),
                candidate(2, 0.9),
                candidate(3, 0.5),
                candidate(4, 0.1)
            ]
        );
        assert_eq!(
            ScoreFilter::new()
                .min_similarity(0.5)
                .top_k(2)
                .apply(scored),
            vec![candidate(1, 0.9), candidate(2, 0.9)]
        );
    }

    #[test]
    fn validation_and_similarity() {
        assert!(ScoreFilter::new()
            .min_similarity(f64::NAN)
            .validate()
            .is_err());
        assert!(ScoreFilter::new().min_similarity(1.5).validate().is_err());
        assert!(ScoreFilter::new().top_k(0).validate().is_err());
        assert!(ScoreFilter::new()
            .min_similarity(0.0)
            .top_k(1)
            .validate()
            .is_ok());
        assert!((signature_similarity(&[1, 2, 3, 4], &[1, 2, 0, 0]) - 0.5).abs() < f64::EPSILON);
        assert!(signature_similarity(&[], &[]).abs() < f64::EPSILON);
        assert!(signature_similarity(&[1], &[1, 2]).abs() < f64::EPSILON);
    }
}
