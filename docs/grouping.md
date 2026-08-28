# Native duplicate grouping

Pari can turn LSH bucket collisions directly into connected duplicate groups in Rust. The large-data path deliberately does **not** build a candidate-edge vector first.

## Direct grouping

`LshIndex32::duplicate_groups()` scans each bucket and unions colliding internal IDs with path compression and union by rank. Auxiliary grouping memory is proportional to the index's internal item slots, not the number of candidate edges.

The unverified path joins each live bucket member to one anchor, so its union work is linear in total bucket memberships rather than quadratic in bucket size. The verified path still visits candidate pairs because acceptance can differ for every pair.

```rust
let groups = index.duplicate_groups();
for group in groups {
    println!("representative={} members={:?}", group.representative(), group.members());
}
```

Groups and members are returned in deterministic sorted order. By default singleton items are omitted.

## Exact verification hook

LSH collisions are candidates, not proof that original application data meets a threshold. `duplicate_groups_with` accepts a callback before disconnected components are joined:

```rust
let groups = index.duplicate_groups_with(2, |left_key, right_key| {
    verify_original_records(left_key, right_key)
});
```

Once two nodes are already connected through accepted edges, Pari skips additional verification between those components. A rejected pair can collide in more than one band and therefore be checked more than once. This tradeoff keeps the main grouping path from retaining an O(candidate edges) rejection cache.

## Candidate pairs

`candidate_pairs()` is available when a caller explicitly needs edges. It streams normalized, unique `(smaller_key, larger_key)` pairs rather than returning a materialized vector. Uniqueness across LSH bands requires retaining a compact internal-ID pair set, so this API can use O(unique candidate pairs) auxiliary memory.

For corpus-scale deduplication where only groups are needed, use direct grouping instead.

## Generic pair grouping

`group_pairs` consumes any iterator of `(u64, u64)` edges and groups them without storing the edge stream:

```rust
let groups = pari_index::group_pairs(edge_iterator, 2);
```

This is useful when candidates originate outside Pari. Self edges introduce their key, duplicate edges are harmless, and memory is O(unique keys).

`group_pairs_with_representative` lets callers choose a representative from the sorted members of each component. Pari validates that the selected representative is actually a member.

## Million-edge smoke benchmark

A dependency-free release-mode workload streams one million chain edges without first allocating an edge vector:

```bash
cargo run --release -p pari-index --example grouping_million
```

This is a reproducible scale smoke test for the grouping algorithm. The broader benchmark harness in issue #8 will add structured timing, RSS, environment metadata, and regression comparison across MinHash, indexing, query, storage, and grouping workloads.
