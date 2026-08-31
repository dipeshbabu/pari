use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use pari_index::LshParams;
use pari_store::PersistentIndex32;

const V1_EMPTY_HEX: &str = include_str!("fixtures/v1-empty.hex");
static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pari-store-compat-{name}-{}-{}.pari",
        std::process::id(),
        NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn decode_hex(input: &str) -> Vec<u8> {
    let compact: String = input
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert_eq!(compact.len() % 2, 0, "fixture hex must contain whole bytes");
    compact
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let value = std::str::from_utf8(pair).expect("fixture hex is ASCII");
            u8::from_str_radix(value, 16).expect("fixture contains valid hex")
        })
        .collect()
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let _ = fs::remove_file(PathBuf::from(temporary));
}

#[test]
fn stable_v1_empty_fixture_opens_with_expected_metadata() {
    let path = test_path("open");
    cleanup(&path);
    let fixture = decode_hex(V1_EMPTY_HEX);
    assert_eq!(fixture.len(), 176, "v1 fixture byte length changed");
    fs::write(&path, fixture).expect("write compatibility fixture");

    let store = PersistentIndex32::open(&path).expect("open stable v1 fixture");
    let stats = store.stats().expect("fixture stats");
    assert_eq!(store.num_perm(), 128);
    assert_eq!(store.seed(), 7);
    assert!((store.threshold() - 0.8).abs() < f64::EPSILON);
    assert_eq!(store.params(), LshParams::new(32, 4));
    assert_eq!(stats.items, 0);
    assert_eq!(stats.bands, 32);
    assert_eq!(stats.rows, 4);
    assert_eq!(stats.committed_buckets, 0);
    assert!(!stats.dirty);
    drop(store);
    cleanup(&path);
}

#[test]
fn current_writer_reproduces_stable_v1_empty_fixture_exactly() {
    let path = test_path("writer");
    cleanup(&path);
    let store = PersistentIndex32::create_with_params(&path, 0.8, 128, 7, LshParams::new(32, 4))
        .expect("create explicit v1 fixture");
    store.close().expect("sync and close fixture");

    let actual = fs::read(&path).expect("read generated fixture");
    assert_eq!(actual, decode_hex(V1_EMPTY_HEX));
    cleanup(&path);
}
