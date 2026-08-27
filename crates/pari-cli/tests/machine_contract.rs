use std::{
    collections::BTreeSet,
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
        "pari-cli-contract-{name}-{}-{nonce}.{extension}",
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

fn parse_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(output.stdout.trim_ascii()).expect("valid JSON output")
}

fn assert_exact_keys(value: &serde_json::Value, expected: &[&str]) {
    let object = value.as_object().expect("JSON object");
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn v01_json_output_fields_are_pinned() {
    let records = temp_path("records", "jsonl");
    let queries = temp_path("queries", "jsonl");
    let index = temp_path("index", "pari");
    fs::write(
        &records,
        concat!(
            "{\"key\":1,\"values\":[\"new york\",\"rust\",\"search\"]}\n",
            "{\"key\":2,\"values\":[\"new york\",\"rust\",\"search\"]}\n",
            "{\"key\":3,\"values\":[\"biology\",\"cell\",\"protein\"]}\n"
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
            records.to_str().expect("records path"),
            "--output",
            index.to_str().expect("index path"),
            "--json",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), &["bands", "file_bytes", "items", "rows"]);

    let output = pari()
        .args([
            "search",
            "--index",
            index.to_str().expect("index path"),
            "--input",
            queries.to_str().expect("queries path"),
            "--json",
        ])
        .output()
        .expect("search command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), &["candidates", "id", "query"]);

    let output = pari()
        .args(["stats", "--index", index.to_str().expect("index path"), "--json"])
        .output()
        .expect("stats command");
    assert_success(&output);
    assert_exact_keys(
        &parse_json(&output),
        &[
            "bands",
            "committed_buckets",
            "dirty",
            "file_bytes",
            "items",
            "num_perm",
            "overlay_buckets",
            "rows",
            "seed",
            "suppressed_base_keys",
            "threshold",
        ],
    );

    let output = pari()
        .args(["verify", "--index", index.to_str().expect("index path"), "--json"])
        .output()
        .expect("verify command");
    assert_success(&output);
    assert_exact_keys(
        &parse_json(&output),
        &["bucket_sections", "buckets", "members_checked", "sections", "valid"],
    );

    let output = pari()
        .args([
            "dedup",
            "--input",
            records.to_str().expect("records path"),
            "--emit",
            "groups",
            "--json",
        ])
        .output()
        .expect("group command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), &["members", "representative"]);

    let output = pari()
        .args([
            "dedup",
            "--input",
            records.to_str().expect("records path"),
            "--emit",
            "pairs",
            "--json",
        ])
        .output()
        .expect("pair command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), &["left", "right"]);

    let _ = fs::remove_file(records);
    let _ = fs::remove_file(queries);
    let _ = fs::remove_file(index);
}
