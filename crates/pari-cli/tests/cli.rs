use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use pari_core::{MinHash32, AFFINE32_SCHEME};

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

fn signature(values: &[&str]) -> Vec<u32> {
    let mut sketch = MinHash32::new(128, 1).expect("valid fixture sketch");
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
}
