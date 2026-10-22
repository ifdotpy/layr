# APFS add and commit, 2026-10-22

The change is in commit `0713d25623a87b97d3a0aefb6c476205d478c3b2`.
The baseline is `b0a41a3f7538c66a8e0e8e97f1fc3030b20abfb4`.
The source hashes identify the exact files used for the candidate build.

## Complete command times

Each value is the median of three runs. Times include command startup and cleanup.
The 10,000-file case changes one file. The 100,000-file case changes ten files.

| Files | Command | Before, seconds | After, seconds | Speedup |
| ---: | --- | ---: | ---: | ---: |
| 10,000 | First add | 2.786 | 1.290 | 2.16x |
| 10,000 | Add with an existing index | 3.635 | 0.199 | 18.23x |
| 10,000 | Commit | 1.421 | 0.445 | 3.20x |
| 100,000 | First add | 71.998 | 24.440 | 2.95x |
| 100,000 | Add with an existing index | 67.380 | 4.573 | 14.73x |
| 100,000 | Commit | 26.293 | 10.260 | 2.56x |

## What changes

- Add copies selected entries into a private tree and validates the source before and after the copy.
- The first add still creates a complete index from the head.
- A later add avoids the full working-tree and index snapshots when validation succeeds.
- An active mmap file retains the snapshot path.
- A regular commit moves the index into the state store. It does not clone and delete the index.
- A journal restores the index if the process stops before the database record commits.
- Commits with overlays retain the existing path.

This is not a sparse persistent index. The first add and commit still have work proportional to the file count.

## Method and limits

Both versions use the same fixture and host. Each trial alternates their order.
Each version gets a fresh branch from the same initial state.
Files have 4,096 bytes. Each phase changes their bytes and restores the original modification time.
The second add runs before commit, with an existing index.
Each run checks staged status, final content, unchanged main content, and fsck.
All twelve branch runs pass these checks. Trace assertions verify the expected clone paths.

The host has other active processes. These are warm runs, not cold-cache measurements.
The 100,000-file candidate times vary as follows:

| Command | Minimum, seconds | Maximum, seconds |
| --- | ---: | ---: |
| First add | 19.069 | 32.112 |
| Add with an existing index | 2.773 | 6.645 |
| Commit | 7.859 | 11.356 |

The original command monitor lost its connection. The benchmark process continued without a restart.
A separate observer confirmed its exit, complete result file, and image removal.
No partial trial is included.

## Validation

- 43 unit tests pass, including four index journal tests.
- The add comparison checks selection, dry run, ignored paths, update mode, file type changes, read-only files, and mmap fallback.
- Real commit tests cover a database rejection, process death, successful index publication, and a refused empty commit.
- Basic commands: 67 checks. Merge: 47. Git: 55. Git compatibility: 27. APFS: 26. All pass.
- The local undo/gc suite passes 30 checks. Its orphan fixture requires the unavailable macOS command `btrfs`.
- GitHub workflow `ci` passes for this commit, including macOS, Linux btrfs, and backup exchange.
- OverlayFS ext4 passes after a retry. The first run fails during test cleanup after all service assertions pass.
- OverlayFS XFS retains its earlier five-second performance failure: 5.247 seconds in this run.

## Reproduce

```sh
python3 benchmarks/apfs-index-20261022/paired.py \
  --baseline /path/to/baseline-layr \
  --candidate /path/to/candidate-layr \
  --root /private/tmp/new-layr-index-benchmark
```

The script creates and removes its own APFS image. The result file remains at the root path.

## Artifacts

- `results.json`: all command times, traces, load samples, binary hashes, and content check results.
- `summary.json`: medians, ranges, and speed ratios.
- `source-sha256.json`: candidate source and test hashes.
- `paired.py`: benchmark script.
