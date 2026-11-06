# Public-corpus validation

The `public-corpora-v1` campaign exercises real inputs through the maintained
text, code, contamination-audit, and entity-matching workloads. It complements
the historical synthetic 100K/1M campaign; it does not replace the dedicated
native-Linux 10M decision gate or establish a larger scaling envelope.

## Sources and preprocessing

The committed [manifest](../benchmarks/campaigns/public-corpora-v1.json) pins
download URLs, SHA-256 digests, provenance, terms, and every workload parameter.
Downloads fail on content changes instead of silently accepting a new dataset.

| Track | Public input | Preparation |
| --- | --- | --- |
| Text deduplication | [UCI SMS Spam Collection](https://archive.ics.uci.edu/dataset/228/sms+spam+collection), 5,574 messages | Decode UTF-8; split each nonempty physical line once at the class/message tab; retain message text and stable row identity. Spam/ham labels are not duplicate labels. |
| Code deduplication | [CPython Lib snapshot](https://github.com/python/cpython/tree/60403a5409ff2c3f3b07dd2ca91a7a3e096839c7/Lib), commit `60403a5409ff2c3f3b07dd2ca91a7a3e096839c7` | Sort regular `.py` members by path; retain repository/file identity; omit files over 256 KiB, non-UTF-8 files, and empty/binary files. Never extract or execute downloaded source. Record every filter count. |
| Contamination audit | The same SMS collection with a deterministic split | Hold out the final 256 source identities; index the other 5,318. Query the first 256 reference messages plus the holdout. Natural duplicates can cross the identity split; exact-feature lookup independently establishes all matches. |
| Entity matching | [Leipzig Abt-Buy](https://dbs.uni-leipzig.de/research/projects/benchmark-datasets-for-entity-resolution), 1,081 Abt and 1,092 Buy products | Decode the CSV files as Latin-1; prefix IDs by source; map `name` to `title` and an available `manufacturer` to `brand`. Use only the distributor's 1,097 original cross-source match pairs as labeled truth. |

SMS and Abt-Buy are distributed under CC BY 4.0 by their cited providers.
Credit Almeida and Hidalgo, SMS Spam Collection (2011), DOI
[10.24432/C5CC84](https://doi.org/10.24432/C5CC84); and the Leipzig database group
of Prof. Erhard Rahm, with Kopcke, Thor, and Rahm, *Evaluation of Entity Resolution
Approaches on Real-World Match Problems*, VLDB 2010. CPython uses the PSF license
and per-file notices; the exact upstream license is linked in the manifest.
The preparation transforms the source data as described above. Raw corpora and
downloaded source remain outside Git; compact reports contain measurements,
checksums, and source identities, with attribution in the bundle manifest.

## Running the campaign

Build and install a wheel from the exact clean Pari commit being measured. The
campaign's [dependency file](../benchmarks/requirements-public-corpora.txt) pins
Datasketch, NumPy, and SciPy; use Python 3.13 for these benchmark-tool versions.
Product interpreter/compiler support remains governed by the normal contributor
setup. Do not compare reports from materially different environments as a
speedup claim.

```sh
python -m pip install -r benchmarks/requirements-public-corpora.txt
python benchmarks/public_corpora.py prepare \
  --cache dist/public-corpora/cache --directory dist/public-corpora/data
python benchmarks/public_corpora.py run \
  --data dist/public-corpora/data --directory dist/public-corpora/run-001
python benchmarks/public_corpora.py validate --directory dist/public-corpora/run-001
```

Output directories must be new. Existing cache bytes are verified, never
overwritten. The runner refuses a dirty checkout and checks the SHA and clean
state again after completion. Each track runs in its own process with a fixed
timeout; a failed track leaves diagnostic files but never a validated bundle.
Use a new output directory for a rerun. Preparation and downloads are excluded
from the timed product workloads; filesystem caches are not reset.

The text and code runs use their existing exact shingle verifiers and persistent
indexes. Text uses normalized Unicode word 3-shingles; code uses the documented
language-neutral lexical 5-shingles with string/number categories normalized.
Literal-byte duplicates and feature-identical duplicates are reported separately.
Empty feature rows, if any, are counted; their feature-level Jaccard is 1 for two
empty sets, which is not a claim of semantic equivalence between the raw inputs.
The entity path produces candidates, not a trained entity matcher. The audit
requires exact shingle equality and compares every returned match with an
independent exhaustive feature-hash lookup.

## Baselines and correctness gates

For every text/code/entity corpus, compare explicit affine32 and affine64
signatures, index construction/sync, querying, index size, bucket distribution,
and exact verification. Report the two widths independently. Datasketch uses
the same features, seed, count, native permutation arrays, and bands/rows;
signature bytes must match exactly. Its candidate sets are reported separately
because implementations use different band-hashing code. Scalar/batch and
reopened-index candidates must match Pari's original result.

A full-corpus Python feature-hash baseline counts exact feature equality.
SciPy's exact binary Jaccard implementation is an additional correctness
baseline on the first 128 records in deterministic input order. It is checked
against independent Python set Jaccard. Its subset timing is never compared
with full-corpus throughput; a zero-denominator recall is reported as unknown,
not perfect recall. Approximate candidate recall can legitimately be below 1.

The original workload's groups must equal connected components reconstructed
independently from the measured candidates and verifier. Entity precision/recall
uses original bipartite labels: no implied transitive or within-source matches
are invented. Poor candidate recall must stay visible rather than being hidden
behind a reduction-ratio headline.

The entity report retains the default 0.4 result and also measures the declared
0.2 and 0.1 thresholds. This is dataset-specific sensitivity analysis, not a
trained or held-out entity-matching score and not an automatic default change.

Each report stores product-workload time and throughput, native construction,
query/verification times, candidate rate, grouping results, persisted bytes,
and checksums of generated outputs. Product-phase peak RSS is captured before
the additional analyses; whole-worker peak RSS also includes retained feature
sets and baselines. Neither is a per-width allocator breakdown. The bundle
records exact Git SHA, clean state, CPU, RAM, filesystem, tool versions, package
versions, commands, configuration, and report checksums.

Keep `bundle.json` and the four track reports as compact immutable evidence
under `benchmarks/results/public-corpora/<source-sha>/`. Retain full scratch
outputs separately when needed; their checksums remain in the reports. Publish
an interpretation alongside the bundle, including limitations and any measured
follow-up work. A newer source SHA gets a new evidence path.
