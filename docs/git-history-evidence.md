# Git history validation

Measured on 2026-10-26 in the dedicated `layr-gh` OrbStack machine (Ubuntu,
AArch64, Btrfs). Source repositories were copied without hardlinks; the source
branches were read only. Mirrors are local bare repositories. No production
Git mirror or AWS store was modified.

## Correctness

`t49-git-history.sh` passes 28 checks. Its five-commit merge fixture includes raw
commit headers and message bytes, a tracked ignored file, binary content,
executable mode, symlink, and Gitlink. It checks all original commit IDs after an
encrypted store transfer and fresh clone, the exact tree, a new child's parent,
repeat import, refused non-fast-forward pushes, and explicit force.

The suite also kills a 64-state push before the first metadata checkpoint,
resumes it, and verifies that existing ciphertext objects are unchanged and all
64 states are complete. It checks incremental file-type changes and corrupts a
loose Git blob before exporting the original IDs again.

`cargo test --workspace --locked`: 225 passed, 7 intentionally ignored.
The requested store suites pass: t19 (25), t22 (7), t23 (4), t35 (49), t42 (99).
Git suites t03 (57), t08 (28), t17 (9) also pass. Counts are assertions, with zero
failures. The lead owns the full-suite check after integration.

## Initial measurements and profile

For layr's 392 commits at `c1d7229a30aa808fedc0274e961b2f38e7cc2f7d`, the first
correct implementation took 402.3 seconds to push and 275.7 seconds to clone in
a debug build. Its release build took 26.1 seconds to push and 65.7 seconds to
clone. The release push spent 12.34 seconds rewriting the complete pushed-state
cache, 8.66 seconds uploading states, and 1.57 seconds rebuilding content indexes.

Incremental hashes, parent snapshots, bounded local tree caches and metadata
checkpoints reduced the 392-commit release push to 11.34 seconds. Cloning parents
also reduced fresh Btrfs space used from 1,756,188,672 to 183,930,880 bytes.
The first clone profile then exposed 82.93 seconds in 4,240 individual Git object
publications, dominated by filesystem flushes. Atomic publication of rebuildable
cache objects reduced a subsequent clone to 19.59 seconds. These samples were
collected during development and include competing VM builds in some runs.

For the 7,965-commit layr-studio copy, import took 558.5 seconds, with maximum RSS
377,820 KiB. The first store-cache optimization still took 1,103.7 seconds to
push. A partial profile at 5,859 states showed 193.3 seconds decoding parent
cache files, 212.5 seconds encoding trees, 109.6 seconds persisting cache files,
and 63.4 seconds checking secrets. The next revision keeps the preceding parent
in memory and emits identical canonical tree bytes directly from typed entries.
The same encoder is used by the reader; canonicality, path, frame, and signature
checks remain in place.

A full Git cache scan on the 7,965-commit source took 13.01 seconds in one sample
and 9.35 seconds in another. Normal bridge commands therefore hash objects they
read. Full scanning and cache reconstruction occur after a failed read or hash
check. The corrupted-cache regression verifies exact IDs after repair.

Final transfer measurements and the complete real-size round-trip proof are
recorded after the active benchmark finishes.
