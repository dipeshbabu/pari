# MinHash core

Pari's first signature implementation is a safe-Rust MinHash core with 32-bit and 64-bit affine permutation families.

## What is inherited from datasketch

The mathematical construction is derived in part from the MIT-licensed `ekzhu/datasketch` version 2 MinHash implementation:

- SHA-1 input hashes use the first 4 or 8 digest bytes in little-endian order.
- Inputs are pre-mixed with the width-appropriate MurmurHash3 finalizer.
- Each permutation uses wrapping affine arithmetic `a * h + b mod 2^w`.
- Multipliers are always odd, making each affine map bijective over the fixed-width integer domain.

The upstream copyright and license notice are preserved in `NOTICE`.

## Intentional compatibility difference

Pari does **not** treat equal Datasketch seeds as byte-for-byte compatibility. Datasketch uses NumPy's legacy `RandomState` mapping from a seed to permutation parameters. Pari specifies SplitMix64 as part of its own scheme so the seed-to-permutation mapping is independent of NumPy and stable for Rust, Python bindings, the CLI, and persisted Pari indexes.

Datasketch 2.x uses the same SHA-1 widths, MurmurHash3 pre-mix, and wrapping affine arithmetic. Signatures are therefore exact when Datasketch is constructed with Pari's explicit multiplier and offset arrays. The optional adapter verifies those complete arrays and rejects ordinary equal-seed sketches; see [Datasketch 2.x interoperability](datasketch-v2.md).

The scheme identifiers are therefore explicit:

- `pari-affine32-v1`
- `pari-affine64-v1`

The migration layer is explicit and does not change either Pari scheme identifier or seed mapping.

## Why no legacy mode

Pari starts without datasketch's pre-2.0 legacy permutation scheme, Python pickle compatibility, custom Python hash callables, or GPU state. Those features add compatibility and security complexity without helping the initial goal of a native, persistent, batch-first similarity engine.

## API behavior

Both `MinHash32` and `MinHash64` support:

- deterministic construction from `num_perm` and `seed`
- `update`
- `update_many`
- ordered `from_batch` and `from_batch_with` construction with bounded CPU parallelism
- explicit `from_signature` reconstruction after the caller validates the
  matching named scheme and seed
- `jaccard`
- `merge`
- `clear`
- zero-copy signature access
- stable multiplier and offset access for checked interoperability

Comparison and merge fail before producing a result when seeds or permutation counts differ.

Batch construction builds each permutation family once and shares those immutable arrays across the returned sketches. See [CPU parallelism](parallelism.md) for the thread policy, crossover evidence, and configuration API.
