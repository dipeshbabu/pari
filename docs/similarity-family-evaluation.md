# Weighted MinHash and SimHash evaluation

## Decision

Defer both families. Weighted MinHash has a clear semantic use for non-negative frequency vectors, but Pari has no reference workload whose answer depends on feature magnitude. SimHash is compact and separates the checked code clone, but the existing MinHash path already solves that fixture and the project has no cosine/Hamming benchmark large enough to justify another index and persistence family.

This is a defer decision, not a rejection. The evidence and required abstraction boundaries below define what must be true before either implementation is proposed again.

## Semantic boundary

Pari's current `MinHash32` estimates set Jaccard. The frequency evaluation contains two vectors with identical support:

| Case | Binary Jaccard | Weighted Jaccard |
| --- | ---: | ---: |
| Same support, different frequency | 1.000 | 0.143 |
| Similar frequency | 1.000 | 0.952 |
| Different support | 0.000 | 0.000 |

Binary MinHash cannot distinguish the first two cases because converting a bag to a set discards counts. Consistent weighted sampling is therefore a separate similarity family, not a flag on `MinHash32`. Ioffe's improved consistent sampling gives matching-sample probability equal to weighted Jaccard and constant work per non-zero weight. See [Ioffe, 2010](https://research.google/pubs/improved-consistent-sampling-weighted-minhash-and-l1-sketching/) and the independent [consistent weighted sampling report](https://www.microsoft.com/en-us/research/publication/consistent-weighted-sampling/).

SimHash uses random hyperplanes for cosine-oriented similarity. A 64-bit deterministic prototype gives similarity 1.000 for the normalized checksum clone and 0.469 for both unrelated code pairs. This is promising, but it is not interchangeable with Jaccard and should not reuse a Jaccard threshold. The construction comes from [Charikar, 2002](https://doi.org/10.1145/509907.509965).

## Workload evidence

The current text, code, and entity workloads all construct feature sets. Their normalizers intentionally deduplicate features before MinHash, and their labels do not demonstrate a frequency-sensitive failure. The code and entity fixtures already achieve full candidate recall with the existing path.

`benchmarks/similarity_family_evaluation.py` adds two narrowly appropriate checks:

- a weighted event-frequency workload where set conversion is provably insufficient;
- the checked-in code clone fixture using a 64-bit frequency-weighted SimHash prototype.

Run it with:

```bash
python benchmarks/similarity_family_evaluation.py --output family-evaluation.json
```

The script records exact semantics, prototype compute time, 8-byte SimHash size, and pair-level quality. These tiny workloads establish meaning and API separation. They are not enough to establish production throughput, memory, or threshold quality.

## Required architecture for Weighted MinHash

An implementation must introduce explicit types such as `WeightedMinHash` and `WeightedLshIndex`; it must not accept weighted input through `MinHash.update`.

Required work:

- a named weighted signature scheme with deterministic seed/randomness rules and golden fixtures;
- non-negative finite-weight validation and explicit sparse-vector input;
- a weighted-Jaccard method that rejects ordinary MinHash signatures;
- an index whose bucket codec understands the weighted sample representation;
- new persisted metadata/sections or a new format version without reinterpreting version 1;
- Python names that keep weighted and set similarity distinct;
- a labeled frequency workload with candidate recall, signature size, build/query throughput, and bytes/item.

Ioffe samples normally retain a pair of values per permutation, so the straightforward representation is materially larger than Pari's current 4-byte-per-permutation signature. Compression choices must be benchmarked rather than assumed.

## Required architecture for SimHash

An implementation must introduce `SimHash64` (or an explicitly sized equivalent) and a Hamming index. Reusing `LshIndex32` would conflate band-collision probability for Jaccard with Hamming-radius search.

Required work:

- a stable feature hash, bit width, weight rules, and tie behavior;
- explicit cosine/Hamming similarity methods and thresholds;
- a multi-index or another bounded Hamming candidate structure with deterministic merging;
- new algorithm/signature identifiers and persistence compatibility checks;
- collision/adversarial tests and a labeled workload large enough to tune radius versus recall;
- comparison against the current MinHash path on signature bytes, compute throughput, index bytes, query cost, and candidate reduction.

The 8-byte prototype is much smaller than a 128-permutation `MinHash32` signature (512 bytes), but signature size alone does not account for Hamming index replication or candidate explosion.

## Revisit criteria

Implement Weighted MinHash only after a checked-in workload contains meaningful non-negative feature magnitudes and shows that set Jaccard loses required ranking or recall.

Implement SimHash only after a cosine/Hamming workload shows a material end-to-end advantage over MinHash, including its index cost, at acceptable recall. Until then, adding either family would expand the public API, format matrix, migration surface, and test burden without solving a demonstrated Pari workload.

The exact evaluation for this decision is recorded in [`evaluation.json`](../benchmarks/results/similarity-families/50ead5c0d2555e7e1d3436876a5d66f4c7ba7926/evaluation.json).
