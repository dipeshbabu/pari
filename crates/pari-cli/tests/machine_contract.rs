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

struct Fixture {
    records: PathBuf,
    queries: PathBuf,
    index: PathBuf,
}

impl Fixture {
    fn new() -> Self {
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
        Self {
            records,
            queries,
            index,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.records);
        let _ = fs::remove_file(&self.queries);
        let _ = fs::remove_file(&self.index);
    }
}

fn assert_index_contract(fixture: &Fixture) {
    let output = pari()
        .args([
            "index",
            "--input",
            fixture.records.to_str().expect("records path"),
            "--output",
            fixture.index.to_str().expect("index path"),
            "--json",
        ])
        .output()
        .expect("index command");
    assert_success(&output);
    assert_exact_keys(
        &parse_json(&output),
        &["bands", "file_bytes", "items", "rows"],
    );
}

fn assert_search_contract(fixture: &Fixture) {
    let output = pari()
        .args([
            "search",
            "--index",
            fixture.index.to_str().expect("index path"),
            "--input",
            fixture.queries.to_str().expect("queries path"),
            "--json",
        ])
        .output()
        .expect("search command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), &["candidates", "id", "query"]);
}

fn assert_stats_contract(fixture: &Fixture) {
    let output = pari()
        .args([
            "stats",
            "--index",
            fixture.index.to_str().expect("index path"),
            "--json",
        ])
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
}

fn assert_verify_contract(fixture: &Fixture) {
    let output = pari()
        .args([
            "verify",
            "--index",
            fixture.index.to_str().expect("index path"),
            "--json",
        ])
        .output()
        .expect("verify command");
    assert_success(&output);
    assert_exact_keys(
        &parse_json(&output),
        &[
            "bucket_sections",
            "buckets",
            "members_checked",
            "sections",
            "valid",
        ],
    );
}

fn assert_dedup_contract(fixture: &Fixture, emit: &str, expected: &[&str]) {
    let output = pari()
        .args([
            "dedup",
            "--input",
            fixture.records.to_str().expect("records path"),
            "--emit",
            emit,
            "--json",
        ])
        .output()
        .expect("dedup command");
    assert_success(&output);
    assert_exact_keys(&parse_json(&output), expected);
}

#[test]
fn v01_json_output_fields_are_pinned() {
    let fixture = Fixture::new();
    assert_index_contract(&fixture);
    assert_search_contract(&fixture);
    assert_stats_contract(&fixture);
    assert_verify_contract(&fixture);
    assert_dedup_contract(&fixture, "groups", &["members", "representative"]);
    assert_dedup_contract(&fixture, "pairs", &["left", "right"]);
}
