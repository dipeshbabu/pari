use std::time::Instant;

use pari_index::group_pairs;

fn main() {
    const EDGE_COUNT: u64 = 1_000_000;

    let started = Instant::now();
    let groups = group_pairs((0..EDGE_COUNT).map(|key| (key, key + 1)), 2);
    let elapsed = started.elapsed();

    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0].len(),
        usize::try_from(EDGE_COUNT + 1).expect("edge count fits usize")
    );
    println!(
        "grouped {EDGE_COUNT} streamed edges into {} component(s) in {elapsed:?}",
        groups.len()
    );
}
