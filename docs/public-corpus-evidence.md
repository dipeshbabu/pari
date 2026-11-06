# Public-corpus evidence, 6 November 2026

All four `public-corpora-v1` tracks completed their correctness gates on clean
commit [`d7b55e9287a255aa9b756bf0521f2212d50c8bae`](https://github.com/dipeshbabu/pari/commit/d7b55e9287a255aa9b756bf0521f2212d50c8bae).
Its source tree matches the squash merge of [PR #208](https://github.com/dipeshbabu/pari/pull/208).
The immutable [bundle](../benchmarks/results/public-corpora/d7b55e9287a255aa9b756bf0521f2212d50c8bae/bundle.json),
[checksums](../benchmarks/results/public-corpora/d7b55e9287a255aa9b756bf0521f2212d50c8bae/SHA256SUMS),
[build provenance](../benchmarks/results/public-corpora/d7b55e9287a255aa9b756bf0521f2212d50c8bae/build-provenance.json),
and [component observations](../benchmarks/results/public-corpora/d7b55e9287a255aa9b756bf0521f2212d50c8bae/observations.json)
retain the commands, source/input hashes, configuration, environment, and result
hashes. The [campaign guide](public-corpora.md) describes acquisition, licenses,
filtering, baselines, and all correctness gates.

## Environment and comparison scope

This was one local Windows 11 / NTFS run on an Intel Family 6 Model 158 CPU with
12 logical CPUs and 16 GiB RAM. The volume had about 95.1 GiB free after the run.
Python was 3.13.12, Datasketch 2.0.0, NumPy 2.5.4, and SciPy 1.18.1. The measured
development wheel identifies itself as 0.1.2 and was built from the clean source
commit above with Rust 1.97.1 and maturin 1.14.1, as recorded separately in build
provenance. The bundle's host `rustc` field reports the shell default, 1.96.0;
that is not the compiler used to build the measured wheel.

Native signature work used two threads, 64 permutations, seed 7, and batches of
256. Filesystem caches were warm and were not reset. These are bounded local
observations with no repeated-run confidence intervals. They establish useful
correctness and workload evidence; the dedicated native-Linux scaling envelope
remains the historical 1M result until the 10M campaign is run.

Datasketch used the same native permutation arrays and LSH bands/rows. All six
signature and candidate-set comparisons matched. Its index is in memory; Pari's
build measurements include persistent creation and sync. Signature-phase
comparisons share feature semantics. Index-build timings have different
durability guarantees and should be interpreted separately. The Python exact
feature-hash baseline uses the full corpus; the independent SciPy Jaccard
baseline uses only the first 128 records. Its timing is a subset measurement.

## Text and code deduplication

| Workload | Accepted inputs | End-to-end workload seconds | Product-phase peak RSS | LSH candidate pairs | Exact-verified lexical pairs | Verified groups | Records dropped |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| SMS messages | 5,574 | 1.294 | 45.9 MiB | 1,275 | 1,249 | 330 | 493 |
| CPython library files | 1,699 | 24.411 | 92.3 MiB | 372 | 293 | 36 | 102 |

The text run found 984 raw-message-identical pairs and 1,161 feature-identical
pairs. The code run found 108 raw-file-identical pairs and 199 feature-identical
pairs. Normalization and lexical similarity account for the additional verified
pairs; this does not establish semantic equivalence of programs. Groups are
connected components, so not every pair within a component must directly meet
the verifier threshold.

The code input contained 1,726 regular Python files before filtering: five
exceeded 256 KiB, three used other encodings, and 19 were empty or binary. All
repository/file identities were retained. This snapshot contains one repository;
the report measures within-repository duplication and makes no cross-project
contamination claim. The component observations retain examples from the three
largest groups without copying source code into the evidence bundle.

The largest verified text group contained 30 messages; the largest code group
contained 11 files. Group-size p95 was four for text and ten for code. The
affine32 bucket maxima were 30 and 11, respectively. The old synthetic campaign
uses query-item candidate rates while these deduplication runs count unique
unordered pairs, so those rate columns should not be combined into a scale ratio.

## Contamination audit

The persistent reference contains 5,318 SMS messages. The 512 queries comprise
256 deliberately repeated reference messages and 256 held-out source identities.
The held-out identities can still contain natural duplicate text.

The audit produced 528 candidates and retained 517 exact-feature matches across
284 queries, agreeing with the independent exhaustive feature-hash lookup.
The verifier rejected 11 candidates. Thus 28 held-out identities also matched
reference features naturally. Candidate reduction was 99.9806% against the full
query/reference Cartesian product. Exact overlap here means equality of the
documented normalized shingle sets, not byte identity or a general proof of
training-data contamination.

Reference construction took 5.060s and the audit invocation took 0.299s. Its
internal measured query phase was 0.0943s; those scopes include different startup,
reopen, and output work. The reference index occupied 1,248,704 bytes and the
audit worker's peak RSS was 50.2 MiB.

## Labeled product matching

Abt-Buy contains 1,081 Abt records, 1,092 Buy records, and 1,097 original labeled
cross-source matches. Abt's CSV omits the manufacturer field; preprocessing
preserves that absence instead of inventing a brand. The experiment uses the
existing product title/brand feature policy.

| LSH threshold | Cross-source candidates | Labeled matches retrieved | Candidate recall | Candidate precision | Pair reduction |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0.4, default | 1,820 | 408 | 37.19% | 22.42% | 99.85% |
| 0.2 | 31,846 | 983 | 89.61% | 3.09% | 97.30% |
| 0.1 | 91,400 | 1,072 | 97.72% | 1.17% | 92.26% |

For this dataset, callers prioritizing recall can use the existing
`--threshold 0.1` option and budget for substantially more downstream comparisons.
The default profile loses many labeled matches. A high pair-reduction ratio is
not an entity-matching quality guarantee. Even the high-recall setting has many
false candidates; use a domain verifier or classifier before merging products.
These are explicit sensitivity measurements on one benchmark, not a trained or
held-out model score or a change to the library's defaults.

The default all-source candidate graph contains a component of 675 records.
Candidate groups are deliberately unverified and must not be treated as gold
entity clusters. Label scoring uses only the provider's original bipartite
pairs, with no transitive or within-source labels added. The default product
workload took 1.148s with product-phase peak RSS of 46.1 MiB.

## Signature widths, cost, and follow-up decisions

| Corpus | affine32 candidates | affine64 candidates | affine32 index bytes | affine64 index bytes |
| --- | ---: | ---: | ---: | ---: |
| Text | 1,275 | 1,253 | 1,307,648 | 1,308,064 |
| Code | 372 | 423 | 408,568 | 408,984 |
| Products, all sources | 6,175 | 6,991 | 1,525,752 | 1,518,744 |

Both widths passed scalar/batch queries, persisted reopen, Datasketch signature
parity, and independent workload-group reconstruction. Raw 64-permutation
signature storage is 256 bytes per item for affine32 and 512 for affine64.
The persisted indexes are similar in size because they store band memberships,
not a second full signature array. Different candidate counts alone do not
establish which family has better recall. These corpora provide no general
quality reason to replace the affine32 default; affine64 remains useful for its
explicit full-width and interoperability contract.

The independent 128-record SciPy sample found one positive Jaccard pair each in
text and code, both retrieved, and 48 in product features, of which affine32
retrieved 38. The small text/code denominators support a correctness check, not
a broad near-duplicate recall claim.

The secondary affine32 code analysis took 1.161s for signatures, 0.210s for
construction/sync, 0.0312s for exact set verification, and 0.00087s for grouping.
The original end-to-end reference workload took 24.411s. The combined scopes
include different feature extraction, source-cache access, and output work;
profile those paths before attributing the gap to CPU or I/O. Exact set
arithmetic alone was small. The CLI's spanning-component verifier also checks
fewer pairs than the secondary all-candidate analysis, so their pair counters
have different meanings.

The reference workloads retain lightweight record references. Their modest peak
RSS does not establish the cost of retaining arbitrary application records in
`DedupeIndex`, nor prove out-of-core operation. Whole-worker peaks, which include
the additional retained feature sets and baselines, were 136.6 MiB for text,
588.6 MiB for code, and 142.5 MiB for products. Those are not per-width allocator
measurements.

The evidence supports the current interfaces and identifies concrete tuning and
profiling work without requiring a public API, format, planner, or storage
architecture change before this patch release. Keep these timing observations in
the workload report; they do not justify a universal README speedup claim.
The native-Linux 128 GiB campaign in [#95](https://github.com/dipeshbabu/pari/issues/95)
and the gated out-of-core evaluation in [#96](https://github.com/dipeshbabu/pari/issues/96)
remain open. The available local host is Windows with 16 GiB RAM, and the
repository has no configured self-hosted runner for that decision run.
