# Persistent local indexes

Pari's local persistent indexes are designed around explicit committed generations. `PersistentIndex32` and `PersistentIndex64` keep compact lookup metadata in memory, read committed bucket memberships from disk on demand, and store mutations in an in-memory overlay until the next commit. The types are deliberately distinct: the 32-bit type accepts only `pari-affine32-v1`, while the 64-bit type accepts only `pari-affine64-v1`.

The local backend is intended to cover the space between an in-memory index and a shared remote backend. It is deliberately a single-writer file format rather than a database or a cross-process coordination protocol.

## Stable format

The current local format uses the versioned `PARIIDX` container from `pari-format` with required `Keys`, `BandHashes`, and one or more canonical `Buckets` sections. Large key catalogs split `Keys` and `BandHashes` into several row-aligned pairs of about 64 MiB each, so persistent indexes are not limited by the 256 MiB per-section cap; see the [index format](index-format.md).

Bucket membership is split into sorted, independently checksummed segments. Reopening validates metadata and bucket directories but does not reconstruct every bucket membership into hash maps. Candidate member ranges are read and verified only when a query touches them.

Read-only lazy handles retain a sorted, compact `u64` live-key catalog after
checking its count, checksum, and uniqueness. Unordered unique legacy catalogs
remain readable. Queries filter stale member IDs through this catalog, matching
the mutable reader's live-key behavior. Complete bucket verification streams
checksums with a bounded read buffer.

Configuration validation does not create an in-memory LSH index. A clean
persistent handle defers its per-band mutation tables until the first insertion;
opening an empty advanced-parameter index therefore does not allocate those
tables merely to validate the header.

The stable v1 compatibility fixtures live at:

```text
crates/pari-store/tests/fixtures/v1-empty.hex
crates/pari-store/tests/fixtures/v1-empty-affine64.hex
```

CI verifies both directions:

1. the current reader can open those fixed v1 bytes;
2. the current writer reproduces those bytes exactly for the same empty-index metadata.

Changing those bytes is therefore a persisted-format compatibility change, not an incidental refactor.

## Stored signatures and similarity scores

LSH alone returns candidate keys. To rank candidates by similarity, create the
index with stored signatures:

```rust
use pari_store::{PersistentIndex32, PersistentIndexOptions, ScoreFilter};

let mut index = PersistentIndex32::create_with_options(
    "documents.pari",
    0.8,
    128,
    7,
    PersistentIndexOptions::new().store_signatures(true),
)?;
index.insert(1, &sketch)?;
for candidate in index.query_scored(&query, ScoreFilter::new().min_similarity(0.8).top_k(10))? {
    println!("{} {:.3}", candidate.key, candidate.similarity);
}
```

`query_scored` returns only LSH candidates, each with the fraction of equal
signature values (the standard MinHash Jaccard estimate), sorted from most to
least similar with ties broken by key. `ScoreFilter::min_similarity` drops
candidates below a threshold and `ScoreFilter::top_k` limits the result count.
`signature(key)` returns a stored sketch, and `stores_signatures()` reports the
setting. Indexes created without signatures return
`StoreError::SignaturesUnavailable` from scored queries.

Signatures cost `num_perm × 4` bytes per item for affine32 and `num_perm × 8`
for affine64 on disk. Queries read candidate rows lazily, so reopening does not
load them. A commit materializes every live signature once, alongside the
snapshot encoding the commit already performs, and reuses that copy to recover
in memory if publication fails. The setting is fixed at creation and persists
across reopen; signatures cannot be added to an existing index after the fact.

Signatures are stored in optional `Signatures` sections (see the
[index format](index-format.md)). Pari releases that predate them skip the
sections when reading. Committing such a file with an older release rewrites
it without signatures, after which scored queries report that signatures are
unavailable. The bounded external builder copies signature sections unchanged.

Read-only `LazyIndex32` and `LazyIndex64` answer the same scored queries from a
file written with stored signatures: `query_scored`, `query_scored_many`, and
`stores_signatures` read only the candidate rows they score. Files without
signature sections return `LazyStoreError::SignaturesUnavailable`.

## Writer model

Use one persistent-index writer for an index path at a time.

Pari never lets one handle silently overwrite another handle's commit. Every
commit with changes takes an exclusive operating-system lock on a sibling
`<index>.lock` file, waiting while another commit to the same path finishes,
and then checks that the snapshot on disk is still the one this handle opened
or last committed. If another handle, in this or any other process, committed
in between, the commit fails with an I/O error stating that the index was
committed by another handle. The refused handle keeps its uncommitted changes
in memory and keeps refusing; reopen the index and reapply them. A handle with
nothing to commit never takes the lock.

Each commit removes the `<index>.lock` file and releases the lock before it
returns, so the file exists only while a commit is running. This is conflict detection, not a
multi-writer transaction: if several processes need to mutate one logical
index, coordinate ownership outside Pari or use a shared backend designed for
that purpose. Pari releases earlier than this one do not take the lock.

Inserts and removals are visible immediately through the writer that performed them because queries merge the committed generation with the in-memory mutation overlay. They are not durable until a successful commit.

The overlay stores the first key of each uncommitted bucket inline and allocates a list only for buckets that hold more than one key, so a singleton bucket costs one hash-map entry.

## Delta commits and compaction

A commit appends the changes since the last commit (inserted keys with their
band hashes, signatures and bucket memberships, plus the keys removed) as a
delta generation at the end of the file, instead of rewriting the snapshot.
The cost of a small commit is therefore proportional to the change, not to
the index. Queries look up each band in the snapshot and in every generation
and accept a member only from the generation that holds the key's current
entry, so results are identical to a rewritten snapshot.

A commit rewrites the whole snapshot instead (the original commit path, now
also called compaction) when:

- 32 delta generations already exist;
- the appended generations would exceed a quarter of the snapshot's size;
- removed or re-inserted keys leave more hidden memberships than a quarter of
  the live keys;
- the file holds a legacy or empty snapshot, or the previous commit's outcome
  was uncertain.

Bulk ingest that commits often can use `flush_delta()` (Rust) instead of
`flush()`. It appends the pending changes whatever their size, so committing a
growing index batch by batch never rewrites it, and rewrites only once 1,024
generations exist or removed keys hide more than a quarter as many memberships
as there are live keys. Queries look up every generation, so call `compact()`
when the ingest finishes, or `compact_and_close()` when the handle is no longer
needed: it writes the same snapshot and consumes the handle without reopening
the rewritten file. If that rewrite fails, every change committed before the
call stays in the file. `DedupeIndex(backend="local")` works this way: it
commits each batch of about 65,536 records as a delta generation and calls
`compact_and_close()` on `close()`.

`compact()` rewrites on demand, folding every generation into one snapshot and
dropping removed keys' memberships. `stats()` reports `delta_generations` and
`delta_bytes`; `suppressed_base_keys` counts the hidden memberships a
compaction would drop. `pari compact --index PATH` does the same from the
command line.

A snapshot rewrite merges the committed generations instead of rebuilding the
index from scratch. Every generation stores its keys in ascending order and
its buckets in `(band, hash)` order, so the rewrite merges those sorted streams
and drops memberships that are no longer live. It splits the bucket key space
into ranges at directory-block boundaries, merges the ranges on all CPU cores
with positioned reads, and writes them in order. It verifies every directory
block and member list it reads, and the result is byte-for-byte the snapshot a
full rebuild would write. Memory stays at a few read buffers per generation
plus the ranges in flight, instead of every live key's band hashes. Files
whose generations do not store keys in order, written by older releases, and
rewrites with many uncommitted keys use the full rebuild.

The format of delta records is specified in
[index-format.md](index-format.md#appended-commit-generations). Pari 0.3 and
earlier cannot open a file with delta generations; compact it first if an older
release must read it.

## Memory after open

Opening a current snapshot, through a persistent index, `ReadOnlyIndex`,
`LazyIndex32`/`LazyIndex64`, or `pari verify`, reads and validates every
bucket directory but keeps only a compact index in memory: the high 32 bits
of every bucket hash, the first key and CRC32 of each block of up to 64
same-band directory entries (at most 2 KiB on disk), and an exact histogram of
bucket sizes for statistics. That is about 4.6 bytes per bucket instead of the
56-byte in-memory location used before. Most band keys that match no bucket
are rejected from memory; otherwise the query reads and checksums the one
directory block that can hold the key, then reads that bucket's members. A persistent index
additionally keeps the set of live keys and, for keys inserted since the last
commit, their band hashes. Committed keys' band hashes stay on disk: a snapshot
rewrite streams them from the generations that store them into the new
snapshot.

## Reader model and file lifetime

An opened local index owns a file handle to the committed generation it opened. A separate reader does not automatically observe a generation committed later by another process. Reopen the index to observe the latest committed target.

An appended commit only adds bytes after the data an open reader already
validated, so readers keep answering from the generations they opened. A
compaction replaces the file by atomic rename, like any full snapshot commit.

Atomic replacement behavior while unrelated processes hold the target open is ultimately subject to operating-system and filesystem semantics. Pari does not currently promise portable lock-free concurrent writer plus reader replacement on every filesystem. Deployments that require that guarantee should coordinate reader refreshes at the application level.

## Creation

`PersistentIndex32::create` and `PersistentIndex64::create` write and sync the complete initial snapshot before atomically claiming an absent final name. Creation never replaces an existing file, directory, or symlink, including a broken symlink or a destination created while the snapshot is being written. A collision returns an `AlreadyExists` I/O error and removes only the transaction-owned temporary file. Successful creation syncs the parent directory under the same platform policy as `sync()`.

## `flush`

When `flush()` appends a delta generation, it:

1. discards any bytes left after the last committed generation by an
   interrupted commit;
2. writes the delta record header and sections and syncs the file;
3. writes the commit trailer and syncs the file;
4. reads the new generation back, checks it holds exactly the pending changes,
   and makes it part of the handle's committed state.

The commit point is the synced trailer. If writing fails before it, the
changes stay pending in memory and the next commit tries again. If the outcome
is uncertain, the handle keeps whatever is committed on disk, reports the
error, and its next commit rewrites the snapshot.

When `flush()` rewrites the snapshot, it:

1. encode the next complete snapshot;
2. atomically claim a uniquely named sibling temporary file and write into it;
3. flush and `sync_all` the temporary file;
4. close Pari's handle to the previous committed generation;
5. atomically rename the temporary path over the target;
6. reopen and validate the committed target.

`flush()` does **not** fsync the containing directory. It provides an atomic file-generation boundary, but a sudden machine or filesystem failure immediately after the rename can still leave directory-entry durability dependent on the filesystem.

If the index is already clean, `flush()` is a no-op.

## `sync`

`sync()` performs the same commit sequence as `flush()` and then `sync_all`s the containing directory on non-Windows platforms. Rust's standard library does not expose a portable Windows directory `fsync`, so Windows returns after the synced file rename instead of treating an unsupported directory open as a write failure.

Use `sync()` when the caller needs the strongest durability guarantee the local backend currently exposes.

If the parent-directory sync reports an error after the rename, Pari returns that error and marks the reopened handle dirty. The target may already contain the new generation, but the caller must treat durability as unconfirmed rather than silently assuming success.

Calling `sync()` on a clean index still syncs the containing directory where the platform supports that operation. On Windows it is a successful no-op because the committed file was already synced before rename.

## `close`

`close()` consumes the index handle and calls `sync()` first. A successful `close()` therefore has the same durability contract as a successful `sync()`.

Dropping a dirty `PersistentIndex32` or `PersistentIndex64` without calling `flush`, `sync`, or `close` does not implicitly commit the in-memory overlay.

## Crash and interrupted-write behavior

The target path is the only committed state. Transaction-owned sibling temporary files are never treated as committed state.

If a process stops while appending a delta generation, the record has no valid
commit trailer: readers ignore it and reopen the previous generation, and the
next commit overwrites it. A record whose trailer is valid was made durable
before the trailer was written, so any damage to it is reported as
corruption.

If a process stops while writing the temporary file, reopening the target uses the previous committed generation. The same rule applies even when the temporary file contains a complete, valid, fsynced newer snapshot but the atomic rename never happened.

Temporary files are claimed with exclusive creation, so a commit never follows or truncates a pre-existing file or symlink. Pari removes the temporary name after success and ordinary failures. A stale file left by a process crash is ignored by readers and cannot be reused by a later commit attempt.

After the atomic rename succeeds, the new target is the logical committed generation. For power-loss durability of that directory entry, use `sync()` rather than `flush()`.

## Corruption behavior

Pari does not silently recover around corrupt committed data. Version metadata, signature scheme and width, section bounds, outer checksums, bucket directories, and bucket member ranges are validated and return typed errors when invalid. Cross-width opens fail before mutation or query.

Directory corruption is detected during open. A corrupt member range that was not needed during open is detected when a query reads that bucket. `LazyIndex32::verify()` and `LazyIndex64::verify()` can be used when a caller wants to read and validate complete bucket sections proactively.

## Backup and copy

For a consistent portable backup:

1. stop mutations or otherwise quiesce the writer;
2. call `sync()` or `close()` successfully, or `compact()` if the copy must be
   readable by Pari 0.3 or earlier;
3. copy the committed target file.

Do not copy the `.tmp` file as an index generation. Avoid racing a backup copy with a writer commit because the copy can observe platform-dependent replacement behavior.

A copied committed target is self-contained. It does not depend on sidecar database files.

## Bounded construction

Mutable commits encode one metadata or bucket section at a time. They sort
bucket memberships in batches of bands or hash ranges with at most 128 MiB of
sorting scratch space. A bucket that exceeds the sort buffer is written on its
own when its payload exceeds the normal segment target. The version-1 section
size limit still applies. Total memory also includes the live key catalog,
pending mutations, and any signatures gathered for recovery.

`PersistentIndex32` and `PersistentIndex64` are optimized for incremental local mutation: small commits append delta generations, and the snapshot is rewritten only when the generations grow. For large initial builds, use `pari-store-build`, which spills fixed-width band-hash records into bounded sorted runs and externally merges them into the same canonical segmented format used by the local and lazy readers. Both the reference and bounded builders accept either supported signature family and preserve the source scheme and width exactly.

A source with appended delta generations is folded while it is read: each generation's keys are ascending, so the builder merges the base and every generation as sorted streams, applies tombstones and re-inserts in commit order, and writes the live keys, band hashes, and signatures laid out exactly as a compaction would. The output is byte-for-byte what compacting and then building produces, the source is not modified, and memory stays bounded by one small read buffer per generation. A generation whose keys are not ascending, which only very old snapshots can have, is rejected with a request to compact first.

When the source is a compacted snapshot, it already holds the bucket sections the external merge would write. With `BuildOptions::reuse_source_buckets`, on by default, the builder copies them instead. It first checks, in bounded memory, that their layout, checksums, ordering and segment breaks are exactly what the rebuild would produce, and that a randomly keyed hash of every `(band, hash, key)` entry matches the source band hashes. If any check fails, it rebuilds the buckets, so the output is identical either way. The contents check is probabilistic; set the option to `false` to always re-derive the buckets.

The external builder's memory contract is bounded by the configured spill buffer, merge heap, one bounded segment directory, fixed copy buffers, and metadata rather than total bucket membership.

Mutable commits and lazy snapshot creation sync the complete temporary file before atomic rename, then sync the parent directory on non-Windows platforms. Bounded external builds provide the same durability while publishing only to an absent destination: they sync a transaction-owned adjacent file, atomically claim the final name without replacement, remove the temporary name, and then sync the parent directory. A post-publication cleanup or directory-sync failure rolls back the final name only while it still identifies the transaction's file, preserving any concurrent replacement.

## Current limitations

The v1 local backend intentionally does not provide multi-writer transactions, merging of concurrent commits, remote replication, TTL, or automatic reader refresh. Those belong in pluggable/shared storage backends rather than being hidden inside the local file format.
