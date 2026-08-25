use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use pari_core::MinHash32;
use pari_store::PersistentIndex32;

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pari-store-recovery-{name}-{}-{}.pari",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(temporary_path(path));
}

fn sketch(base: u64) -> MinHash32 {
    let mut sketch = MinHash32::new(128, 7).expect("valid sketch");
    for value in base..base + 40 {
        sketch.update(&value.to_le_bytes());
    }
    sketch
}

#[test]
fn fsynced_new_generation_before_rename_is_not_committed() {
    let target = test_path("target");
    let newer = test_path("newer");
    cleanup(&target);
    cleanup(&newer);

    let first = sketch(0);
    let second = sketch(10_000);

    let mut committed =
        PersistentIndex32::create(&target, 0.8, 128, 7).expect("create committed store");
    committed.insert(10, &first).expect("insert committed key");
    committed.sync().expect("commit first generation");
    drop(committed);
    let committed_bytes = fs::read(&target).expect("read committed generation");

    let mut candidate =
        PersistentIndex32::create(&newer, 0.8, 128, 7).expect("create newer generation");
    candidate.insert(10, &first).expect("insert first key");
    candidate.insert(20, &second).expect("insert second key");
    candidate.sync().expect("sync newer generation");
    drop(candidate);

    let newer_bytes = fs::read(&newer).expect("read newer generation");
    let temp = temporary_path(&target);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .expect("open target temporary generation");
    file.write_all(&newer_bytes)
        .expect("write complete newer generation to temp");
    file.flush().expect("flush newer temp generation");
    file.sync_all().expect("fsync newer temp generation");
    drop(file);

    let reopened = PersistentIndex32::open(&target).expect("reopen last committed generation");
    assert!(reopened.contains(10));
    assert!(!reopened.contains(20));
    assert_eq!(reopened.len(), 1);
    assert_eq!(
        fs::read(&target).expect("reread committed target"),
        committed_bytes
    );
    drop(reopened);

    cleanup(&target);
    cleanup(&newer);
}
