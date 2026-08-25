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

Pari does **not** attempt byte-for-byte signature compatibility with datasketch. Datasketch uses NumPy's legacy `RandomState` mapping from a seed to permutation parameters. Pari specifies SplitMix64 as part of its own scheme so the seed-to-permutation mapping is independent of NumPy and stable for Rust, Python bindings, the CLI, and persisted Pari indexes.

The scheme identifiers are therefore explicit:

- `pari-affine32-v1`
- `pari-affine64-v1`

A future datasketch migration layer, if needed, should be implemented as an explicit compatibility feature rather than silently changing these scheme semantics.

## Why no legacy mode

Pari starts without datasketch's pre-2.0 legacy permutation scheme, Python pickle compatibility, custom Python hash callables, or GPU state. Those features add compatibility and security complexity without helping the initial goal of a native, persistent, batch-first similarity engine.

## API behavior

Both `MinHash32` and `MinHash64` support:

- deterministic construction from `num_perm` and `seed`
- `update`
- `update_many`
- `jaccard`
- `merge`
- `clear`
- zero-copy signature access

Comparison and merge fail before producing a result when seeds or permutation counts differ.
