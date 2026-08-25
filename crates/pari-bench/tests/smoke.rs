use pari_bench::{run_benchmark, BenchmarkConfig};

#[test]
fn quick_end_to_end_benchmark_preserves_correctness() {
    let report = run_benchmark(BenchmarkConfig {
        items: 64,
        queries: 8,
        set_size: 20,
        overlap: 20,
        threshold: 0.8,
        num_perm: 128,
        seed: 7,
        dataset: None,
    })
    .expect("quick benchmark should run");

    assert_eq!(report.engine, "pari");
    assert_eq!(report.metrics["index.live_items"].value, 64.0);
    assert_eq!(report.metrics["candidate.recall"].value, 1.0);
    assert!(report.metrics["query.scalar_p99_ms"].value >= 0.0);
    assert!(report.metrics["grouping.stream_edges_per_second"].value > 0.0);
}
