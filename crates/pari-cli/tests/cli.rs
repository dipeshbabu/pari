use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

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

#[test]
fn index_search_stats_verify_and_dedup_work_end_to_end() {
    let records = temp_path("records", "jsonl");
    let queries = temp_path("queries", "jsonl");
    let index = temp_path("index", "pari");
    fs::write(
        &records,
        concat!(
            "{\"key\":1,\"values\":[\"new york\",\"rust\",\"search\"]}\n",
            "{\"key\":2,\"values\":[\"new york\",\"rust\",\"search\"]}\n",
            "{\"key\":3,\"values\":[\"biology\",\"protein\",\"cell\"]}\n"
        ),
    )
    .expect("records");
    fs::write(
        &queries,
        "{\"id\":\"q1\",\"values\":[\"new york\",\"rust\",\"search\"]}\n",
    )
    .expect("queries");

    let output = pari()
        .args([
            "index",
            "--input",
            records.to_str().expect("path"),
            "--output",
            index.to_str().expect("path"),
            "--batch-size",
            "2",
            "--json",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).expect("summary JSON");
    assert_eq!(summary["items"], 3);

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
    let candidates = result["candidates"].as_array().expect("candidates");
    assert!(candidates.contains(&serde_json::json!(1)));
    assert!(candidates.contains(&serde_json::json!(2)));

    let output = pari()
        .args(["stats", "--index", index.to_str().expect("path"), "--json"])
        .output()
        .expect("stats command");
    assert_success(&output);
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stats JSON");
    assert_eq!(stats["items"], 3);

    let output = pari()
        .args(["verify", "--index", index.to_str().expect("path"), "--json"])
        .output()
        .expect("verify command");
    assert_success(&output);
    let verified: serde_json::Value = serde_json::from_slice(&output.stdout).expect("verify JSON");
    assert_eq!(verified["valid"], true);
    assert!(verified["members_checked"].as_u64().unwrap_or(0) > 0);

    let output = pari()
        .args([
            "dedup",
            "--input",
            records.to_str().expect("path"),
            "--emit",
            "groups",
            "--json",
        ])
        .output()
        .expect("dedup command");
    assert_success(&output);
    let line = output.stdout.split(|byte| *byte == b'\n').next().expect("group line");
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
fn completion_is_generated_from_command_definition() {
    let output = pari()
        .args(["completion", "bash"])
        .output()
        .expect("completion command");
    assert_success(&output);
    let text = String::from_utf8(output.stdout).expect("utf8 completion");
    assert!(text.contains("pari"));
    assert!(text.contains("dedup"));
    assert!(text.contains("verify"));
}
