use pari_backend::{
    conformance::exercise_backend_contract, BackendCapabilities, BackendCapability, BackendIndex32,
    IndexDescriptor, MemoryBackend, StorageBackend,
};
use pari_core::MinHash32;
use pari_index::LshIndex32;

fn sketch(values: &[&[u8]], num_perm: usize, seed: u64) -> MinHash32 {
    let mut sketch = MinHash32::new(num_perm, seed).expect("valid sketch");
    sketch.update_many(values);
    sketch
}

#[test]
fn public_backend_extension_types_are_constructible() {
    let capabilities = BackendCapabilities::empty()
        .with(BackendCapability::BatchRead)
        .with(BackendCapability::Health);
    assert!(capabilities.supports(BackendCapability::BatchRead));
    assert!(capabilities.supports(BackendCapability::Health));
    assert!(!capabilities.supports(BackendCapability::Remote));

    let params = LshIndex32::new(0.8, 64, 1)
        .expect("reference index")
        .params();
    let descriptor =
        IndexDescriptor::new(0.8, 64, 1, params, None).expect("public descriptor constructor");
    assert_eq!(descriptor.num_perm(), 64);
    assert_eq!(descriptor.seed(), 1);
    assert_eq!(descriptor.params(), params);
}

#[test]
fn memory_backend_satisfies_shared_contract() {
    let backend = MemoryBackend::new();
    let capabilities = backend.capabilities();
    assert!(capabilities.supports(BackendCapability::BatchRead));
    assert!(capabilities.supports(BackendCapability::BatchWrite));
    assert!(!capabilities.supports(BackendCapability::Ttl));
    assert!(!capabilities.supports(BackendCapability::Remote));
    exercise_backend_contract(backend);

    let value = sketch(&[b"distribution"], 64, 1);
    let mut index = BackendIndex32::create(MemoryBackend::new(), 0.8, 64, 1, None)
        .expect("memory diagnostics index");
    index.insert(1, &value).expect("memory diagnostics insert");
    let stats = index.stats().expect("memory stats");
    assert!(
        stats
            .bucket_distribution
            .expect("memory distribution")
            .buckets
            > 0
    );
    index.cleanup().expect("memory cleanup");
}

#[cfg(feature = "redis")]
mod redis_tests {
    use std::{
        process, thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use pari_backend::{
        BackendCapability, BackendIndex32, BackendIndexError, RedisBackend, StorageBackend,
    };

    use super::{exercise_backend_contract, sketch};

    fn redis_url() -> Option<String> {
        std::env::var("PARI_REDIS_URL").ok()
    }

    fn namespace(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        format!("test-{label}-{}-{nanos}", process::id())
    }

    fn clean_backend(url: &str, namespace: &str) -> RedisBackend {
        let mut backend = RedisBackend::connect(url, namespace).expect("connect Redis");
        backend.cleanup().expect("pre-test cleanup");
        backend
    }

    #[test]
    fn redis_backend_satisfies_shared_contract_and_is_shared_across_handles() {
        let Some(url) = redis_url() else {
            eprintln!("PARI_REDIS_URL is not set; skipping Redis integration test");
            return;
        };
        let namespace = namespace("contract");
        let backend = clean_backend(&url, &namespace);
        assert!(backend.capabilities().supports(BackendCapability::Remote));
        assert!(backend.capabilities().supports(BackendCapability::Ttl));

        exercise_backend_contract(backend);

        let backend = clean_backend(&url, &namespace);
        let first = sketch(&[b"alpha", b"beta", b"gamma"], 128, 7);
        let mut index =
            BackendIndex32::create(backend, 0.8, 128, 7, None).expect("shared Redis index");
        index.insert(1, &first).expect("shared Redis insert");
        let reopened_backend =
            RedisBackend::connect(&url, &namespace).expect("second Redis handle");
        let mut reopened = BackendIndex32::open(reopened_backend).expect("open shared namespace");
        assert_eq!(
            reopened.query(&first).expect("query through second handle"),
            vec![1]
        );
        let stats = reopened.stats().expect("Redis stats");
        assert_eq!(stats.items, 1);
        assert!(stats.round_trips > 0);
        drop(index);
        reopened.cleanup().expect("Redis cleanup");
    }

    #[test]
    fn redis_namespaces_are_isolated_and_cleanup_is_scoped() {
        let Some(url) = redis_url() else {
            eprintln!("PARI_REDIS_URL is not set; skipping Redis integration test");
            return;
        };
        let left_namespace = namespace("left");
        let right_namespace = namespace("right");
        let left_backend = clean_backend(&url, &left_namespace);
        let right_backend = clean_backend(&url, &right_namespace);
        let value = sketch(&[b"same"], 64, 11);

        let mut left = BackendIndex32::create(left_backend, 0.8, 64, 11, None).expect("left");
        let mut right = BackendIndex32::create(right_backend, 0.8, 64, 11, None).expect("right");
        left.insert(1, &value).expect("left insert");
        right.insert(2, &value).expect("right insert");
        left.cleanup().expect("left cleanup");

        assert!(right.contains(2).expect("right remains live"));
        right.cleanup().expect("right cleanup");
    }

    #[test]
    fn redis_ttl_expires_the_complete_namespace() {
        let Some(url) = redis_url() else {
            eprintln!("PARI_REDIS_URL is not set; skipping Redis integration test");
            return;
        };
        let namespace = namespace("ttl");
        let backend = clean_backend(&url, &namespace);
        let value = sketch(&[b"ttl"], 64, 3);
        let mut index = BackendIndex32::create(backend, 0.8, 64, 3, Some(Duration::from_secs(1)))
            .expect("TTL index");
        index.insert(1, &value).expect("TTL insert");
        let ttl = index
            .stats()
            .expect("TTL stats")
            .ttl_seconds_remaining
            .expect("TTL must be active");
        assert!(ttl <= 1);
        drop(index);

        thread::sleep(Duration::from_millis(2_200));
        let backend = RedisBackend::connect(&url, &namespace).expect("reconnect after TTL");
        let error = BackendIndex32::open(backend).expect_err("expired namespace must be absent");
        assert!(matches!(
            error,
            BackendIndexError::Backend(pari_backend::BackendError::NotFound)
        ));
    }
}
