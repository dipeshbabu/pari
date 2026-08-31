# Persistent local indexes

Pari's local persistent index is designed around explicit committed generations. `PersistentIndex32` keeps compact lookup metadata in memory, reads committed bucket memberships from disk on demand, and stores mutations in an in-memory overlay until the next commit.

The local backend is intended to cover the space between an in-memory index and a shared remote backend. It is deliberately a single-writer file format rather than a database or a cross-process coordination protocol.

## Stable format

The current local format uses the versioned `PARIIDX` container from `pari-format` with required `Keys`, `BandHashes`, and one or more canonical `Buckets` sections.

Bucket membership is split into sorted, independently checksummed segments. Reopening validates metadata and bucket directories but does not reconstruct every bucket membership into hash maps. Candidate member ranges are read and verified only when a query touches them.

The first stable v1 compatibility fixture lives at:

```text
crates/pari-store/tests/fixtures/v1-empty.hex
```

CI verifies both directions:

1. the current reader can open those fixed v1 bytes;
2. the current writer reproduces those bytes exactly for the same empty-index metadata.

Changing those bytes is therefore a persisted-format compatibility change, not an incidental refactor.

## Writer model

Use one `PersistentIndex32` writer for an index path at a time.

Pari does not take a hidden global or cross-process lock. Two writers targeting the same path can race and are unsupported. If several processes need to mutate one logical index, coordinate ownership outside Pari or use a shared backend designed for that purpose.

Inserts and removals are visible immediately through the writer that performed them because queries merge the committed generation with the in-memory mutation overlay. They are not durable until a successful commit.

## Reader model and file lifetime

An opened local index owns a file handle to the committed generation it opened. A separate reader does not automatically observe a generation committed later by another process. Reopen the index to observe the latest committed target.

Atomic replacement behavior while unrelated processes hold the target open is ultimately subject to operating-system and filesystem semantics. Pari does not currently promise portable lock-free concurrent writer plus reader replacement on every filesystem. Deployments that require that guarantee should coordinate reader refreshes at the application level.

## `flush`

`flush()` commits dirty state in this order:

1. encode the next complete snapshot;
2. write it to the sibling `.tmp` path;
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

Dropping a dirty `PersistentIndex32` without calling `flush`, `sync`, or `close` does not implicitly commit the in-memory overlay.

## Crash and interrupted-write behavior

The target path is the only committed generation. The sibling `.tmp` file is never treated as committed state.

If a process stops while writing the temporary file, reopening the target uses the previous committed generation. The same rule applies even when the temporary file contains a complete, valid, fsynced newer snapshot but the atomic rename never happened.

A stale `.tmp` file can therefore be ignored by readers and is overwritten by a later commit attempt.

After the atomic rename succeeds, the new target is the logical committed generation. For power-loss durability of that directory entry, use `sync()` rather than `flush()`.

## Corruption behavior

Pari does not silently recover around corrupt committed data. Version metadata, section bounds, outer checksums, bucket directories, and bucket member ranges are validated and return typed errors when invalid.

Directory corruption is detected during open. A corrupt member range that was not needed during open is detected when a query reads that bucket. `LazyIndex32::verify()` can be used when a caller wants to read and validate complete bucket sections proactively.

## Backup and copy

For a consistent portable backup:

1. stop mutations or otherwise quiesce the writer;
2. call `sync()` or `close()` successfully;
3. copy the committed target file.

Do not copy the `.tmp` file as an index generation. Avoid racing a backup copy with a writer commit because the copy can observe platform-dependent replacement behavior.

A copied committed target is self-contained. It does not depend on sidecar database files.

## Bounded construction

`PersistentIndex32` is optimized for incremental local mutation and compaction. For large initial builds, use `pari-store-build`, which spills fixed-width records into bounded sorted runs and externally merges them into the same canonical segmented format used by the local and lazy readers.

The external builder's memory contract is bounded by the configured spill buffer, merge heap, one bounded segment directory, fixed copy buffers, and metadata rather than total bucket membership.

Mutable commits, bounded external builds, and lazy snapshot creation use the same platform policy: sync the complete temporary file before atomic rename, then sync the parent directory on non-Windows platforms.

## Current limitations

The v1 local backend intentionally does not provide multi-writer transactions, cross-process locking, remote replication, TTL, or automatic reader refresh. Those belong in pluggable/shared storage backends rather than being hidden inside the local file format.
