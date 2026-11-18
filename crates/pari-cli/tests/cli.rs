use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::{MinHash32, MinHash64, AFFINE32_SCHEME, AFFINE64_SCHEME};
use pari_format::{
    encode_bucket_segment, Algorithm, CodecId, FileLayout, IndexFile, IndexMetadata, Section,
    SectionKind, SignatureScheme,
};

fn pari() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pari"))
}

/// A path unique to this test process and call.
///
/// The counter keeps parallel tests apart even when the clock has not
/// advanced between calls, which happens on Windows' 100 ns clock.
fn temp_path(name: &str, extension: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pari-cli-{name}-{}-{sequence}-{nonce}.{extension}",
        std::process::id()
    ))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn staged_index_paths(index: &Path) -> Vec<PathBuf> {
    let parent = index.parent().expect("index parent");
    let file_name = index
        .file_name()
        .expect("index file name")
        .to_string_lossy();
    let prefix = format!(".{file_name}.pari-cli-");
    fs::read_dir(parent)
        .expect("read index parent")
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect()
}

fn wait_for_staged_index(index: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !staged_index_paths(index).is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("index command did not create its staging file");
}

fn signature(values: &[&str]) -> Vec<u32> {
    let mut sketch = MinHash32::new(128, 1).expect("valid fixture sketch");
    for value in values {
        sketch.update(value.as_bytes());
    }
    sketch.signature().to_vec()
}

fn signature64(values: &[&str], num_perm: usize, seed: u64) -> Vec<u64> {
    let mut sketch = MinHash64::new(num_perm, seed).expect("valid fixture sketch");
    for value in values {
        sketch.update(value.as_bytes());
    }
    sketch.signature().to_vec()
}

#[test]
fn search_text_output_escapes_control_characters_and_json_preserves_ids() {
    let hostile_id = "query\u{1b}[31m\n\r\tend";
    for scheme in [AFFINE32_SCHEME, AFFINE64_SCHEME] {
        let index = temp_path("escaped-query-id", "pari");
        let queries = temp_path("escaped-query-id", "jsonl");
        if scheme == AFFINE32_SCHEME {
            pari_store::PersistentIndex32::create(&index, 0.8, 32, 7)
                .expect("create affine32 index")
                .close()
                .expect("close index");
        } else {
            pari_store::PersistentIndex64::create(&index, 0.8, 32, 7)
                .expect("create affine64 index")
                .close()
                .expect("close index");
        }
        let records = [
            serde_json::json!({"id": hostile_id, "values": ["query"]}).to_string(),
            serde_json::json!({"id": "caf\u{e9}", "values": ["query"]}).to_string(),
        ];
        fs::write(&queries, records.join("\n") + "\n").expect("query records");
        let output = pari()
            .args([
                "search",
                "--index",
                index.to_str().expect("index path"),
                "--input",
                queries.to_str().expect("query path"),
            ])
            .output()
            .expect("search text");
        assert_success(&output);
        let text = String::from_utf8(output.stdout).expect("UTF-8 output");
        assert!(!text.contains(['\u{1b}', '\r']));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "0\tquery\\u{1b}[31m\\n\\r\\tend\t");
        assert_eq!(lines[1], "1\tcaf\u{e9}\t");

        let output = pari()
            .args([
                "search",
                "--index",
                index.to_str().expect("index path"),
                "--input",
                queries.to_str().expect("query path"),
                "--json",
            ])
            .output()
            .expect("search JSON");
        assert_success(&output);
        let text = String::from_utf8(output.stdout).expect("JSON output");
        let first: serde_json::Value =
            serde_json::from_str(text.lines().next().expect("first result")).expect("result JSON");
        assert_eq!(first["id"], hostile_id);
        fs::remove_file(index).expect("cleanup index");
        fs::remove_file(queries).expect("cleanup queries");
    }
}

#[test]
fn dedup_rejects_output_aliases_without_destroying_input() {
    let original = b"{\"key\":1,\"values\":[\"same\"]}\n{\"key\":2,\"values\":[\"same\"]}\n";
    for scheme in [AFFINE32_SCHEME, AFFINE64_SCHEME] {
        for emit in ["pairs", "groups"] {
            for hard_link in [false, true] {
                let input = temp_path("dedup-alias-input", "jsonl");
                fs::write(&input, original).expect("source records");
                let output = if hard_link {
                    let alias = temp_path("dedup-alias-output", "jsonl");
                    fs::hard_link(&input, &alias).expect("input alias");
                    alias
                } else {
                    input.clone()
                };
                let result = pari()
                    .args([
                        "dedup",
                        "--input",
                        input.to_str().expect("input path"),
                        "--output",
                        output.to_str().expect("output path"),
                        "--emit",
                        emit,
                        "--signature-scheme",
                        scheme,
                    ])
                    .output()
                    .expect("dedup command");
                assert_eq!(result.status.code(), Some(2));
                assert!(String::from_utf8_lossy(&result.stderr).contains("input and output"));
                assert_eq!(fs::read(&input).expect("preserved input"), original);
                assert_eq!(fs::read(&output).expect("preserved alias"), original);
                if hard_link {
                    fs::remove_file(output).expect("cleanup alias");
                }
                fs::remove_file(input).expect("cleanup input");
            }
        }
    }
}

#[test]
fn verify_rejects_valid_frames_with_invalid_index_structure() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            scheme,
            CodecId::U64,
            32,
            7,
            0.8,
            8,
            4,
            0,
        )
        .expect("valid metadata");
        let count_section = |kind, count: u64| {
            Section::new(kind, true, count.to_le_bytes().to_vec()).expect("section")
        };
        let invalid_sections = [
            Vec::new(),
            vec![count_section(SectionKind::Keys, 0)],
            vec![
                count_section(SectionKind::Keys, 1),
                count_section(SectionKind::BandHashes, 0),
            ],
            vec![
                count_section(SectionKind::Keys, 0),
                count_section(SectionKind::BandHashes, 0),
                count_section(SectionKind::Keys, 0),
            ],
        ];
        for sections in invalid_sections {
            let path = temp_path("invalid-index-structure", "pari");
            let bytes = IndexFile::new(metadata.clone(), sections)
                .expect("valid container framing")
                .encode()
                .expect("valid frame checksums");
            fs::write(&path, &bytes).expect("malformed snapshot");
            let output = pari()
                .args(["verify", "--index", path.to_str().expect("path"), "--json"])
                .output()
                .expect("verify snapshot");
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            assert_eq!(fs::read(&path).expect("unmodified snapshot"), bytes);
            fs::remove_file(path).expect("cleanup");
        }
    }
}

#[test]
fn verify_checks_unknown_optional_sections_without_breaking_forward_compatibility() {
    for scheme in [
        SignatureScheme::PariAffine32V1,
        SignatureScheme::PariAffine64V1,
    ] {
        let path = temp_path("optional-checksum", "pari");
        let metadata = IndexMetadata::new(
            Algorithm::MinHashLsh,
            scheme,
            CodecId::U64,
            32,
            7,
            0.8,
            8,
            4,
            0,
        )
        .expect("valid metadata");
        let sections = vec![
            Section::new(SectionKind::Keys, true, 0_u64.to_le_bytes().to_vec()).expect("keys"),
            Section::new(SectionKind::BandHashes, true, 0_u64.to_le_bytes().to_vec())
                .expect("hashes"),
            Section::new(SectionKind::Tombstones, false, vec![42; 70_000])
                .expect("future optional section"),
            Section::new(
                SectionKind::Buckets,
                true,
                encode_bucket_segment(&[]).expect("empty buckets"),
            )
            .expect("buckets"),
        ];
        let mut bytes = IndexFile::new(metadata, sections)
            .expect("container")
            .encode()
            .expect("encode container");
        let layout = FileLayout::read_from(&mut std::io::Cursor::new(&bytes))
            .expect("known optional layout");
        let extension = layout.section(SectionKind::Tombstones).expect("extension");
        let offset = usize::try_from(extension.payload_offset()).expect("small offset");
        bytes[offset - 16..offset - 14].copy_from_slice(&999_u16.to_le_bytes());
        fs::write(&path, &bytes).expect("valid future snapshot");
        let output = pari()
            .args(["verify", "--index", path.to_str().expect("path"), "--json"])
            .output()
            .expect("verify future snapshot");
        assert_success(&output);

        bytes[offset] ^= 1;
        fs::write(&path, bytes).expect("corrupt optional payload");
        let output = pari()
            .args(["verify", "--index", path.to_str().expect("path"), "--json"])
            .output()
            .expect("verify corrupt snapshot");
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
        fs::remove_file(path).expect("cleanup");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn index_search_stats_verify_and_dedup_work_end_to_end() {
    let output = pari()
        .args([
            "plan",
            "--items",
            "1000000",
            "--memory-budget-mib",
            "2048",
            "--storage",
            "auto",
            "--json",
        ])
        .output()
        .expect("plan command");
    assert_success(&output);
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).expect("plan JSON");
    assert_eq!(plan["model"], "pari-lsh-planner-v1");
    assert_eq!(plan["bands"], 9);
    assert_eq!(plan["rows"], 13);
    assert_eq!(plan["parameter_source"], "tuned");
    assert_eq!(plan["sizes"]["persistent_index_bytes"], 440_000_736_u64);
    assert_eq!(plan["recommended_storage"], "memory");

    let records = temp_path("records", "jsonl");
    let queries = temp_path("queries", "jsonl");
    let index = temp_path("index", "pari");
    let duplicate_signature = signature(&["new york", "rust", "search"]);
    let records_text = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({"key": 1, "values": ["new york", "rust", "search"]}),
        serde_json::json!({
            "key": 2,
            "signature": duplicate_signature,
            "scheme": AFFINE32_SCHEME,
        }),
        serde_json::json!({"key": 3, "values": ["biology", "protein", "cell"]}),
    );
    fs::write(&records, records_text).expect("records");
    let queries_text = format!(
        "{}\n",
        serde_json::json!({
            "id": "q1",
            "signature": signature(&["new york", "rust", "search"]),
            "scheme": AFFINE32_SCHEME,
        })
    );
    fs::write(&queries, queries_text).expect("queries");

    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
            "--batch-size",
            "2",
            "--progress",
            "json",
            "--json",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).expect("summary JSON");
    assert_eq!(summary["items"], 3);
    assert!(
        staged_index_paths(&index).is_empty(),
        "successful command retained staging files"
    );
    let progress = output
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("progress JSON"))
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[0]["phase"], "index");
    assert_eq!(progress[0]["completed"], 2);
    assert_eq!(progress[1]["completed"], 3);
    assert_eq!(progress[1]["final_event"], true);

    let output = pari()
        .args([
            "search",
            "--index",
            index.to_str().expect("path"),
            "--input",
            queries.to_str().expect("path"),
            "--json",
            "--progress",
            "json",
            "--progress-every",
            "1",
        ])
        .output()
        .expect("search command");
    assert_success(&output);
    let result: serde_json::Value =
        serde_json::from_slice(output.stdout.trim_ascii()).expect("search JSON");
    let candidates = result["candidates"].as_array().expect("candidates");
    assert!(candidates.contains(&serde_json::json!(1)));
    assert!(candidates.contains(&serde_json::json!(2)));
    let search_progress = output
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("progress JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        search_progress.last().expect("final progress")["final_event"],
        true
    );
    assert!(
        search_progress.last().expect("final progress")["candidate_rate"]
            .as_f64()
            .unwrap_or(0.0)
            > 0.0
    );

    let output = pari()
        .args(["stats", "--index", index.to_str().expect("path"), "--json"])
        .output()
        .expect("stats command");
    assert_success(&output);
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stats JSON");
    assert_eq!(stats["items"], 3);
    assert_eq!(stats["committed_bucket_distribution"]["exact"], true);
    assert!(
        stats["committed_bucket_distribution"]["memberships"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );

    let output = pari()
        .args([
            "explain",
            "--index",
            index.to_str().expect("path"),
            "--json",
        ])
        .output()
        .expect("explain command");
    assert_success(&output);
    let explanation: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(explanation["items"], 3);
    assert_eq!(explanation["parameter_source"], "existing");
    assert_eq!(explanation["requested_storage"], "persistent");
    assert_eq!(explanation["bands"], stats["bands"]);
    assert_eq!(explanation["rows"], stats["rows"]);

    let output = pari()
        .args([
            "verify",
            "--index",
            index.to_str().expect("path"),
            "--json",
            "--progress",
            "json",
            "--progress-every",
            "1",
        ])
        .output()
        .expect("verify command");
    assert_success(&output);
    let verified: serde_json::Value = serde_json::from_slice(&output.stdout).expect("verify JSON");
    assert_eq!(verified["valid"], true);
    assert!(verified["members_checked"].as_u64().unwrap_or(0) > 0);
    assert!(!output.stderr.is_empty());

    let output = pari()
        .args([
            "dedup",
            "--input",
            records.to_str().expect("path"),
            "--emit",
            "groups",
            "--json",
            "--batch-size",
            "2",
            "--progress",
            "json",
        ])
        .output()
        .expect("dedup command");
    assert_success(&output);
    let line = output
        .stdout
        .split(|byte| *byte == b'\n')
        .next()
        .expect("group line");
    let group: serde_json::Value = serde_json::from_slice(line).expect("group JSON");
    assert_eq!(group["members"], serde_json::json!([1, 2]));

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(queries);
    let _ = fs::remove_file(index);
}

#[test]
#[allow(clippy::too_many_lines)]
fn affine64_upper_bits_work_across_every_metadata_driven_command() {
    const NUM_PERM: usize = 16;
    const SEED: u64 = 7;
    let records = temp_path("affine64-records", "jsonl");
    let queries = temp_path("affine64-queries", "jsonl");
    let index = temp_path("affine64-index", "pari");
    let values = ["new york", "rust", "search"];
    let duplicate_signature = signature64(&values, NUM_PERM, SEED);
    assert!(
        duplicate_signature
            .iter()
            .any(|value| *value > u64::from(u32::MAX)),
        "fixture must exercise signature upper bits"
    );
    fs::write(
        &records,
        format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"key": 1, "values": values}),
            serde_json::json!({
                "key": 2,
                "signature": duplicate_signature,
                "scheme": AFFINE64_SCHEME,
                "seed": SEED,
            }),
            serde_json::json!({"key": 3, "values": ["biology", "protein", "cell"]}),
        ),
    )
    .expect("records");
    fs::write(
        &queries,
        format!(
            "{}\n",
            serde_json::json!({
                "id": "q64",
                "signature": signature64(&values, NUM_PERM, SEED),
                "scheme": AFFINE64_SCHEME,
                "seed": SEED,
            })
        ),
    )
    .expect("queries");

    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
            "--signature-scheme",
            AFFINE64_SCHEME,
            "--num-perm",
            "16",
            "--seed",
            "7",
            "--batch-size",
            "1",
            "--progress",
            "json",
            "--json",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    let progress = output
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("progress JSON"))
        .collect::<Vec<_>>();
    assert_eq!(progress.last().expect("final")["completed"], 3);
    assert_eq!(progress.last().expect("final")["final_event"], true);
    let mut file = fs::File::open(&index).expect("index");
    let layout = FileLayout::read_from(&mut file).expect("layout");
    assert_eq!(
        layout.metadata().signature_scheme(),
        SignatureScheme::PariAffine64V1
    );

    let output = pari()
        .args([
            "search",
            "--index",
            index.to_str().expect("path"),
            "--input",
            queries.to_str().expect("path"),
            "--json",
        ])
        .output()
        .expect("search command");
    assert_success(&output);
    let result: serde_json::Value =
        serde_json::from_slice(output.stdout.trim_ascii()).expect("search JSON");
    assert_eq!(result["id"], "q64");
    assert_eq!(result["candidates"], serde_json::json!([1, 2]));

    let output = pari()
        .args(["stats", "--index", index.to_str().expect("path"), "--json"])
        .output()
        .expect("stats command");
    assert_success(&output);
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stats JSON");
    assert_eq!(stats["items"], 3);
    assert_eq!(stats["num_perm"], NUM_PERM);
    assert_eq!(stats["seed"], SEED);

    let output = pari()
        .args([
            "explain",
            "--index",
            index.to_str().expect("path"),
            "--json",
        ])
        .output()
        .expect("explain command");
    assert_success(&output);
    let explanation: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("explain JSON");
    assert_eq!(explanation["items"], 3);
    assert_eq!(
        explanation["sizes"]["signature_bytes_per_item"],
        NUM_PERM * 8
    );

    let output = pari()
        .args(["verify", "--index", index.to_str().expect("path"), "--json"])
        .output()
        .expect("verify command");
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("verify JSON")["valid"],
        true
    );

    for emit in ["pairs", "groups"] {
        let output = pari()
            .args([
                "dedup",
                "--input",
                records.to_str().expect("path"),
                "--signature-scheme",
                AFFINE64_SCHEME,
                "--num-perm",
                "16",
                "--seed",
                "7",
                "--emit",
                emit,
                "--json",
            ])
            .output()
            .expect("dedup command");
        assert_success(&output);
        let result: serde_json::Value =
            serde_json::from_slice(output.stdout.trim_ascii()).expect("dedup JSON");
        if emit == "pairs" {
            assert_eq!(result, serde_json::json!({"left": 1, "right": 2}));
        } else {
            assert_eq!(result["members"], serde_json::json!([1, 2]));
        }
    }

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(queries);
    let _ = fs::remove_file(index);
}

#[test]
fn default_and_explicit_affine32_builds_are_byte_identical() {
    let records = temp_path("affine32-parity-records", "jsonl");
    let default_index = temp_path("affine32-default", "pari");
    let explicit_index = temp_path("affine32-explicit", "pari");
    fs::write(&records, "{\"key\":1,\"values\":[\"stable\",\"bytes\"]}\n").expect("records");

    let build = |index: &Path, explicit: bool| {
        let mut command = pari();
        command.args([
            "index",
            "--input",
            records.to_str().expect("records path"),
            "--output",
            index.to_str().expect("index path"),
        ]);
        if explicit {
            command.args(["--signature-scheme", AFFINE32_SCHEME]);
        }
        command.output().expect("index command")
    };
    assert_success(&build(&default_index, false));
    assert_success(&build(&explicit_index, true));
    assert_eq!(
        fs::read(&default_index).expect("default bytes"),
        fs::read(&explicit_index).expect("explicit bytes")
    );

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(default_index);
    let _ = fs::remove_file(explicit_index);
}

#[test]
fn width_scheme_seed_and_count_mismatches_have_line_context() {
    let cases = [
        (
            serde_json::json!({
                "key": 1,
                "signature": [1, 2],
                "scheme": AFFINE32_SCHEME,
            }),
            AFFINE64_SCHEME,
            "signature scheme",
        ),
        (
            serde_json::json!({
                "key": 1,
                "signature": [1, 2],
                "scheme": AFFINE64_SCHEME,
                "seed": 99,
            }),
            AFFINE64_SCHEME,
            "seed 99",
        ),
        (
            serde_json::json!({
                "key": 1,
                "signature": [1],
                "scheme": AFFINE64_SCHEME,
            }),
            AFFINE64_SCHEME,
            "has 1 values; expected 2",
        ),
        (
            serde_json::json!({
                "key": 1,
                "signature": [u64::from(u32::MAX) + 1, 1],
                "scheme": AFFINE32_SCHEME,
            }),
            AFFINE32_SCHEME,
            "exceeds",
        ),
    ];
    for (case, selected_scheme, expected) in cases {
        let records = temp_path("invalid-width-metadata", "jsonl");
        let index = temp_path("invalid-width-index", "pari");
        fs::write(&records, format!("{case}\n")).expect("records");
        let output = pari()
            .args([
                "index",
                "--input",
                records.to_str().expect("path"),
                "--output",
                index.to_str().expect("path"),
                "--signature-scheme",
                selected_scheme,
                "--num-perm",
                "2",
            ])
            .output()
            .expect("index command");
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("line 1"), "missing line context: {error}");
        assert!(error.contains(expected), "unexpected diagnostic: {error}");
        assert!(!index.exists());
        assert!(staged_index_paths(&index).is_empty());
        let _ = fs::remove_file(records);
    }
}

#[test]
fn affine64_cross_family_query_is_rejected_with_line_context() {
    let records = temp_path("cross-family-records", "jsonl");
    let query = temp_path("cross-family-query", "jsonl");
    let index = temp_path("cross-family-index", "pari");
    fs::write(&records, "{\"key\":1,\"values\":[\"value\"]}\n").expect("records");
    fs::write(
        &query,
        format!(
            "{}\n",
            serde_json::json!({
                "signature": [1, 2],
                "scheme": AFFINE32_SCHEME,
            })
        ),
    )
    .expect("query");
    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
            "--signature-scheme",
            AFFINE64_SCHEME,
            "--num-perm",
            "2",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    let output = pari()
        .args([
            "search",
            "--index",
            index.to_str().expect("path"),
            "--input",
            query.to_str().expect("path"),
        ])
        .output()
        .expect("search command");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("line 1"));
    assert!(error.contains(AFFINE64_SCHEME));

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(query);
    let _ = fs::remove_file(index);
}

#[test]
fn invalid_json_returns_nonzero_with_line_context() {
    let records = temp_path("invalid", "jsonl");
    let index = temp_path("invalid-index", "pari");
    fs::write(&records, "{not-json}\n").expect("fixture");
    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
        ])
        .output()
        .expect("command");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("line 1"));
    let _ = fs::remove_file(records);
    let _ = fs::remove_file(index);
}

#[test]
fn index_failure_after_committed_batch_leaves_no_destination() {
    for scheme in [AFFINE32_SCHEME, AFFINE64_SCHEME] {
        let records = temp_path(&format!("partial-records-{scheme}"), "jsonl");
        let index = temp_path(&format!("partial-index-{scheme}"), "pari");
        fs::write(&records, "{\"key\":1,\"values\":[\"valid\"]}\n{not-json}\n").expect("fixture");

        let output = pari()
            .args([
                "index",
                "--input",
                records.to_str().expect("path"),
                "--output",
                index.to_str().expect("path"),
                "--signature-scheme",
                scheme,
                "--batch-size",
                "1",
            ])
            .output()
            .expect("command");

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("line 2"));
        assert!(!index.exists(), "failed command published a partial index");
        assert!(
            staged_index_paths(&index).is_empty(),
            "failed command retained staging files"
        );
        let _ = fs::remove_file(records);
    }
}

#[test]
fn missing_index_input_leaves_no_destination() {
    let records = temp_path("missing-records", "jsonl");
    let index = temp_path("missing-index", "pari");

    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
        ])
        .output()
        .expect("command");

    assert!(!output.status.success());
    assert!(!index.exists(), "failed command published an empty index");
    assert!(
        staged_index_paths(&index).is_empty(),
        "failed command retained staging files"
    );
}

#[test]
fn concurrent_index_destination_is_not_replaced_or_removed() {
    for scheme in [AFFINE32_SCHEME, AFFINE64_SCHEME] {
        let index = temp_path(&format!("concurrent-index-{scheme}"), "pari");
        let mut child = pari()
            .args([
                "index",
                "--input",
                "-",
                "--output",
                index.to_str().expect("path"),
                "--signature-scheme",
                scheme,
                "--json",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn index command");

        wait_for_staged_index(&index);
        fs::write(&index, b"concurrent owner").expect("concurrent destination");
        let mut stdin = child.stdin.take().expect("child stdin");
        writeln!(stdin, "{{\"key\":1,\"values\":[\"valid\"]}}").expect("input row");
        drop(stdin);

        let output = child.wait_with_output().expect("index command");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("without replacement"));
        assert_eq!(
            fs::read(&index).expect("concurrent destination remains"),
            b"concurrent owner"
        );
        assert!(
            staged_index_paths(&index).is_empty(),
            "failed publication retained staging files"
        );
        let _ = fs::remove_file(index);
    }
}

#[test]
fn invalid_signature_metadata_is_rejected_with_line_context() {
    let records = temp_path("invalid-signature", "jsonl");
    let index = temp_path("invalid-signature-index", "pari");
    fs::write(
        &records,
        "{\"key\":1,\"signature\":[1,2,3],\"scheme\":\"other\"}\n",
    )
    .expect("fixture");
    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
        ])
        .output()
        .expect("command");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("line 1"));
    assert!(error.contains("signature scheme"));
    let _ = fs::remove_file(records);
    let _ = fs::remove_file(index);
}

#[test]
fn completion_is_generated_from_command_definition() {
    let output = pari()
        .args(["completion", "bash"])
        .output()
        .expect("completion command");
    assert_success(&output);
    let text = String::from_utf8(output.stdout).expect("utf8 completion");
    assert!(text.contains("pari"));
    assert!(text.contains("dedup"));
    assert!(text.contains("plan"));
    assert!(text.contains("explain"));
    assert!(text.contains("verify"));
    assert!(text.contains("signature-scheme"));
    assert!(text.contains(AFFINE64_SCHEME));

    for command in ["index", "dedup"] {
        let output = pari()
            .args([command, "--help"])
            .output()
            .expect("help command");
        assert_success(&output);
        let text = String::from_utf8(output.stdout).expect("utf8 help");
        assert!(text.contains("--signature-scheme"));
        assert!(text.contains(AFFINE32_SCHEME));
        assert!(text.contains(AFFINE64_SCHEME));
    }
}

#[test]
fn closed_stdout_pipe_exits_cleanly_without_panicking() {
    use std::fmt::Write as _;

    let mut records = String::new();
    let mut queries = String::new();
    for key in 0..5_000 {
        let group = key % 10;
        writeln!(
            records,
            "{{\"key\":{key},\"values\":[\"shared\",\"group-{group}\"]}}"
        )
        .expect("format record");
        writeln!(queries, "{{\"values\":[\"shared\",\"group-{group}\"]}}").expect("format query");
    }
    let index = temp_path("closed-pipe", "pari");
    let mut build = pari()
        .args(["index", "--output"])
        .arg(&index)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn index");
    build
        .stdin
        .take()
        .expect("index stdin")
        .write_all(records.as_bytes())
        .expect("write index input");
    assert_success(&build.wait_with_output().expect("index output"));

    let index_arg = index.to_string_lossy().into_owned();
    let commands: [(&[&str], &str); 4] = [
        (&["dedup", "--emit", "pairs"], &records),
        (&["search", "--index", &index_arg], &queries),
        (&["stats", "--index", &index_arg], ""),
        (&["completion", "bash"], ""),
    ];
    for (args, input) in commands {
        let mut child = pari()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn command");
        // Close the read end before the command writes anything.
        drop(child.stdout.take());
        let mut stdin = child.stdin.take().expect("command stdin");
        let input = input.to_owned();
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let output = child.wait_with_output().expect("command output");
        writer.join().expect("stdin writer");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{args:?} failed on a closed stdout pipe: {stderr}"
        );
        assert!(!stderr.contains("panicked"), "{args:?} panicked: {stderr}");
    }
    let _ = fs::remove_file(index);
}

#[test]
fn dedup_creates_missing_output_directories() {
    let input = temp_path("dedup-nested-input", "jsonl");
    fs::write(
        &input,
        b"{\"key\":1,\"values\":[\"same\"]}\n{\"key\":2,\"values\":[\"same\"]}\n",
    )
    .expect("source records");
    let root = temp_path("dedup-nested-output", "dir");
    let output = root.join("nested").join("groups.tsv");
    let result = pari()
        .arg("dedup")
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&output)
        .output()
        .expect("dedup command");
    assert_success(&result);
    assert_eq!(
        fs::read_to_string(&output).expect("dedup output"),
        "1\t1,2\n"
    );

    let blocked = temp_path("dedup-blocked-parent", "txt");
    fs::write(&blocked, b"not a directory").expect("blocking file");
    let target = blocked.join("groups.tsv");
    let failed = pari()
        .arg("dedup")
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&target)
        .output()
        .expect("dedup command");
    assert_eq!(failed.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains(&target.display().to_string()),
        "error should name the output path: {stderr}"
    );

    fs::remove_dir_all(root).expect("cleanup output");
    fs::remove_file(blocked).expect("cleanup blocker");
    fs::remove_file(input).expect("cleanup input");
}

#[test]
fn dedup_min_similarity_filters_pairs_and_groups() {
    let input = temp_path("dedup-min-similarity", "jsonl");
    fs::write(
        &input,
        concat!(
            "{\"key\":1,\"values\":[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\",\"j\"]}\n",
            "{\"key\":2,\"values\":[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\",\"k\"]}\n",
            "{\"key\":3,\"values\":[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\",\"j\"]}\n",
        ),
    )
    .expect("records");
    let run = |extra: &[&str]| {
        let output = pari()
            .arg("dedup")
            .arg("--input")
            .arg(&input)
            .args(["--threshold", "0.5"])
            .args(extra)
            .output()
            .expect("dedup");
        assert_success(&output);
        String::from_utf8(output.stdout).expect("utf8")
    };

    assert_eq!(run(&["--emit", "pairs"]).lines().count(), 3);
    assert_eq!(
        run(&["--emit", "pairs", "--min-similarity", "0.95", "--json"]),
        "{\"left\":1,\"right\":3,\"similarity\":1.0}\n"
    );
    assert_eq!(
        run(&["--emit", "pairs", "--min-similarity", "0.95"]),
        "1\t3\t1.0000\n"
    );
    assert_eq!(run(&["--min-similarity", "0.95"]), "1\t1,3\n");

    let invalid = pari()
        .arg("dedup")
        .arg("--input")
        .arg(&input)
        .args(["--min-similarity", "1.5"])
        .output()
        .expect("dedup");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("--min-similarity"));
    let _ = fs::remove_file(input);
}

#[test]
fn batched_index_builds_append_generations_that_verify_and_compact() {
    let records = temp_path("delta-records", "jsonl");
    let index = temp_path("delta-index", "pari");
    let queries = temp_path("delta-queries", "jsonl");
    let mut text = String::new();
    let mut query_text = String::new();
    for key in 0..1_500_u64 {
        let group = key / 3;
        let values: Vec<String> = (0..12)
            .map(|token| format!("g{group}-t{token}"))
            .chain([format!("item-{key}")])
            .collect();
        text.push_str(&serde_json::json!({"key": key, "values": values}).to_string());
        text.push('\n');
        if key % 7 == 0 {
            query_text.push_str(
                &serde_json::json!({"id": format!("q{key}"), "values": values}).to_string(),
            );
            query_text.push('\n');
        }
    }
    fs::write(&records, text).expect("records");
    fs::write(&queries, query_text).expect("queries");
    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
            "--batch-size",
            "100",
            "--threshold",
            "0.5",
        ])
        .output()
        .expect("index command");
    assert_success(&output);

    let verify = |index: &Path| -> serde_json::Value {
        let output = pari()
            .args(["verify", "--index", index.to_str().expect("path"), "--json"])
            .output()
            .expect("verify command");
        assert_success(&output);
        serde_json::from_slice(&output.stdout).expect("verify JSON")
    };
    let stats = |index: &Path| -> serde_json::Value {
        let output = pari()
            .args(["stats", "--index", index.to_str().expect("path"), "--json"])
            .output()
            .expect("stats command");
        assert_success(&output);
        serde_json::from_slice(&output.stdout).expect("stats JSON")
    };
    let before = verify(&index);
    let generations = before["delta_generations"].as_u64().expect("generations");
    assert!(generations > 0, "small batches append delta generations");
    assert_eq!(stats(&index)["delta_generations"], generations);
    assert_eq!(stats(&index)["items"], 1_500);

    let search = |index: &Path, read_only: bool| -> Vec<u8> {
        let mut command = pari();
        command.args([
            "search",
            "--index",
            index.to_str().expect("path"),
            "--input",
        ]);
        command.arg(&queries);
        if read_only {
            command.arg("--read-only");
        }
        let output = command.output().expect("search command");
        assert_success(&output);
        output.stdout
    };
    let results = search(&index, false);
    assert_eq!(search(&index, true), results);

    let output = pari()
        .args(["compact", "--index", index.to_str().expect("path")])
        .output()
        .expect("compact command");
    assert_success(&output);
    let after = verify(&index);
    assert_eq!(after["delta_generations"], 0);
    // A bucket stored by several generations is one record after compaction.
    assert!(after["buckets"].as_u64() <= before["buckets"].as_u64());
    assert_eq!(stats(&index)["delta_bytes"], 0);
    assert_eq!(search(&index, false), results);
    assert_eq!(search(&index, true), results);

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(queries);
    let _ = fs::remove_file(index);
}
