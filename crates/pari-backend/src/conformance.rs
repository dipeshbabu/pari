//! Reusable contract tests for third-party [`StorageBackend`] implementations.
//!
//! Enable the `conformance` feature in a development dependency, then pass a
//! fresh, isolated, uninitialized backend to [`exercise_backend_contract`].
//! The function panics at the first contract violation so it can be called
//! directly from an ordinary Rust test.
//!
//! This kit checks product-independent behavior. Backend-specific integration
//! tests remain responsible for namespace isolation, cross-process visibility,
//! transport failures, and real-time retention expiry.

use std::time::Duration;

use pari_core::MinHash32;
use pari_index::LshIndex32;

use crate::{
    BackendCapabilities, BackendCapability, BackendError, BackendIndex32, BackendIndexError,
    IndexDescriptor, StorageBackend,
};

const NUM_PERM: usize = 128;
const SEED: u64 = 7;
const RETENTION: Duration = Duration::from_secs(300);

/// Exercise the product-independent contract for one storage backend.
///
/// `backend` must refer to a fresh, isolated namespace with no existing Pari
/// descriptor or records. The exercise creates data and removes it with
/// [`StorageBackend::cleanup`] before returning.
///
/// # Panics
///
/// Panics with a contract-focused message if the backend violates descriptor,
/// capability, atomic batch, query, deletion, operational, statistics, or
/// cleanup semantics.
#[track_caller]
pub fn exercise_backend_contract<B: StorageBackend>(backend: B) {
    let capabilities = backend.capabilities();
    assert_required_capabilities(capabilities);
    let retention = capabilities
        .supports(BackendCapability::Ttl)
        .then_some(RETENTION);

    let first = sketch(&[b"alpha", b"beta", b"gamma"]);
    let duplicate = first.clone();
    let third = sketch(&[b"unrelated", b"record"]);

    let mut reference =
        LshIndex32::new(0.8, NUM_PERM, SEED).expect("conformance reference index must be valid");
    let mut index = BackendIndex32::create(backend, 0.8, NUM_PERM, SEED, retention)
        .expect("backend must initialize a fresh namespace");
    let descriptor = index.descriptor().clone();

    exercise_atomicity(&mut index, &mut reference, &first, &duplicate, &third);
    exercise_queries_and_delete(&mut index, &mut reference, &first, &third);
    exercise_operations_and_stats(&mut index, capabilities);

    let mut reopened = reopen(index, &descriptor);
    assert_eq!(
        reopened
            .query(&first)
            .expect("reopened backend query must succeed"),
        reference
            .query(&first)
            .expect("conformance reference reopen query must succeed"),
        "reopened backend candidates differ from LshIndex32"
    );
    exercise_cleanup(reopened.into_backend());
}

#[track_caller]
fn exercise_atomicity<B: StorageBackend>(
    index: &mut BackendIndex32<B>,
    reference: &mut LshIndex32,
    first: &MinHash32,
    duplicate: &MinHash32,
    third: &MinHash32,
) {
    reference
        .insert(1, first)
        .expect("conformance reference insert must succeed");
    index
        .insert(1, first)
        .expect("backend must accept an initial insert");

    let error = index
        .insert_many([(2, duplicate), (1, first)])
        .expect_err("backend must reject a batch containing an existing key");
    assert!(
        matches!(
            error,
            BackendIndexError::Backend(BackendError::DuplicateKey { key: 1 })
        ),
        "backend must report the existing duplicate key; got {error}"
    );
    assert!(
        !index
            .contains(2)
            .expect("backend contains must work after a rejected batch"),
        "backend partially committed a rejected insertion batch"
    );

    index
        .insert_many([(2, duplicate), (3, third)])
        .expect("backend must accept a valid insertion batch");
    reference
        .insert_many([(2, duplicate), (3, third)])
        .expect("conformance reference batch insert must succeed");
}

#[track_caller]
fn exercise_queries_and_delete<B: StorageBackend>(
    index: &mut BackendIndex32<B>,
    reference: &mut LshIndex32,
    first: &MinHash32,
    third: &MinHash32,
) {
    index.set_observability(true);

    assert_eq!(
        index
            .query(first)
            .expect("backend scalar query must succeed"),
        reference
            .query(first)
            .expect("conformance reference scalar query must succeed"),
        "backend scalar candidates differ from LshIndex32"
    );
    assert_eq!(
        index
            .query_many([first, third])
            .expect("backend batch query must succeed"),
        reference
            .query_many([first, third])
            .expect("conformance reference batch query must succeed"),
        "backend batch candidates differ from LshIndex32"
    );
    assert_eq!(
        index
            .contains_many(&[3, 1, 4, 1])
            .expect("backend batch contains must succeed"),
        vec![true, true, false, true],
        "backend contains results must preserve input order"
    );

    assert_eq!(
        index
            .remove_many([2, 99, 2])
            .expect("backend batch delete must succeed"),
        1,
        "backend delete count must include only live distinct keys"
    );
    assert!(reference.remove(2));
    assert_eq!(
        index
            .query(first)
            .expect("backend query after delete must succeed"),
        reference
            .query(first)
            .expect("conformance reference query after delete must succeed"),
        "backend deletion must remove all bucket memberships for the key"
    );
}

#[track_caller]
fn exercise_operations_and_stats<B: StorageBackend>(
    index: &mut BackendIndex32<B>,
    capabilities: BackendCapabilities,
) {
    index.flush().expect("backend flush must succeed");
    index.health().expect("backend health probe must succeed");
    let stats = index.stats().expect("backend statistics must succeed");
    assert_eq!(
        stats.items, 2,
        "backend statistics report the wrong item count"
    );
    assert!(
        stats.bucket_memberships > 0,
        "backend statistics must report live bucket memberships"
    );
    let queries = stats
        .queries
        .expect("BackendIndex32 must attach enabled query observations");
    assert_eq!(queries.operations, 3);
    assert_eq!(queries.queries, 4);
    assert_eq!(queries.possible_candidates, 11);
    assert!(queries.candidate_rate() > 0.0);
    if capabilities.supports(BackendCapability::Ttl) {
        assert!(
            stats.ttl_seconds_remaining.is_some(),
            "a backend advertising TTL must report active retention"
        );
    } else {
        assert_eq!(
            stats.ttl_seconds_remaining, None,
            "a backend without TTL must not report active retention"
        );
    }
}

#[track_caller]
fn reopen<B: StorageBackend>(
    index: BackendIndex32<B>,
    descriptor: &IndexDescriptor,
) -> BackendIndex32<B> {
    let mut backend = index.into_backend();
    let stored_descriptor = backend
        .load_descriptor()
        .expect("backend must load its initialized descriptor");
    assert_eq!(
        &stored_descriptor, descriptor,
        "backend changed descriptor fields while persisting them"
    );
    let error = backend
        .initialize(descriptor)
        .expect_err("backend must reject initialization of an existing namespace");
    assert!(
        matches!(error, BackendError::AlreadyExists),
        "backend must report AlreadyExists when initialized twice; got {error}"
    );
    BackendIndex32::open(backend).expect("backend must reopen its persisted descriptor")
}

#[track_caller]
fn exercise_cleanup<B: StorageBackend>(mut backend: B) {
    backend
        .cleanup()
        .expect("backend cleanup must remove the owned namespace");
    match backend.load_descriptor() {
        Err(BackendError::NotFound) => {}
        Err(error) => panic!("backend must report NotFound after cleanup; got {error}"),
        Ok(_) => panic!("backend descriptor remained available after cleanup"),
    }
}

fn assert_required_capabilities(capabilities: BackendCapabilities) {
    for capability in [
        BackendCapability::BatchRead,
        BackendCapability::BatchWrite,
        BackendCapability::Delete,
        BackendCapability::Flush,
        BackendCapability::Health,
    ] {
        assert!(
            capabilities.supports(capability),
            "backend must advertise required capability {capability:?}"
        );
    }
}

fn sketch(values: &[&[u8]]) -> MinHash32 {
    let mut sketch = MinHash32::new(NUM_PERM, SEED).expect("conformance sketch must be valid");
    sketch.update_many(values);
    sketch
}

#[cfg(test)]
mod tests {
    use pari_format::BucketKey;

    use super::exercise_backend_contract;
    use crate::{
        BackendCapabilities, BackendError, BackendStats, IndexDescriptor, MemoryBackend,
        StorageBackend, StoredItem,
    };

    #[derive(Debug, Default)]
    struct PartialCommitBackend(MemoryBackend);

    impl StorageBackend for PartialCommitBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.0.capabilities()
        }

        fn initialize(&mut self, descriptor: &IndexDescriptor) -> Result<(), BackendError> {
            self.0.initialize(descriptor)
        }

        fn load_descriptor(&mut self) -> Result<IndexDescriptor, BackendError> {
            self.0.load_descriptor()
        }

        fn contains_many(&mut self, keys: &[u64]) -> Result<Vec<bool>, BackendError> {
            self.0.contains_many(keys)
        }

        fn insert_many(&mut self, items: &[StoredItem]) -> Result<(), BackendError> {
            for item in items {
                self.0.insert_many(std::slice::from_ref(item))?;
            }
            Ok(())
        }

        fn query_buckets(&mut self, buckets: &[BucketKey]) -> Result<Vec<Vec<u64>>, BackendError> {
            self.0.query_buckets(buckets)
        }

        fn delete_many(&mut self, keys: &[u64]) -> Result<usize, BackendError> {
            self.0.delete_many(keys)
        }

        fn flush(&mut self) -> Result<(), BackendError> {
            self.0.flush()
        }

        fn health(&mut self) -> Result<(), BackendError> {
            self.0.health()
        }

        fn stats(&mut self) -> Result<BackendStats, BackendError> {
            self.0.stats()
        }

        fn cleanup(&mut self) -> Result<(), BackendError> {
            self.0.cleanup()
        }
    }

    #[test]
    #[should_panic(expected = "backend partially committed a rejected insertion batch")]
    fn conformance_kit_detects_partial_batch_commits() {
        exercise_backend_contract(PartialCommitBackend::default());
    }
}
