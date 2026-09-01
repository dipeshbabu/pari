use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pari_core::{MinHash32, MinHash64, AFFINE32_SCHEME, AFFINE64_SCHEME};
use pari_format::{FileLayout, SignatureScheme};

fn pari() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pari"))
}

fn temp_path(name: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pari-cli-{name}-{}-{nonce}.{extension}",
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
