# APFS optimization results, 2026-10-21

## Changes

- `status` reads the live trees and validates kernel change counters before and after the read.
- Invalid counters, active memory mappings, or changes during the read select the existing snapshot path.
- Equal, valid APFS clone identifiers on the same volume avoid a content comparison. Other files still require a comparison.
- `add` replaces a private index file with a clone. It preserves the destination attributes and applies the source mode and time.
- Hard-linked or flagged destinations use the existing copy path. Working files retain the existing copy behavior.

## Paired measurements

Three trials per binary and case. The order alternates. Both binaries use the same fixture and APFS volume.
Times include the complete command. Allocation includes APFS metadata; `sync` runs outside each timer.

| Files | Command | Before, median s | After, median s | Ratio |
| ---: | --- | ---: | ---: | ---: |
| 10,000 | `branch` | 3.123 | 1.822 | 1.71x |
| 10,000 | `status_clean` | 7.196 | 0.470 | 15.31x |
| 10,000 | `status_dirty` | 4.818 | 0.394 | 12.23x |
| 10,000 | `diff` | 6.036 | 3.040 | 1.99x |
| 10,000 | `add` | 9.244 | 5.784 | 1.60x |
| 10,000 | `status_staged` | 14.298 | 0.883 | 16.20x |
| 10,000 | `commit` | 5.862 | 4.000 | 1.47x |
| 100,000 | `branch` | 18.706 | 19.302 | 0.97x |
| 100,000 | `status_clean` | 52.241 | 2.622 | 19.93x |
| 100,000 | `status_dirty` | 50.241 | 2.503 | 20.07x |

The dirty case changes one 4 KiB file and restores its modification time.
The 100,000-file case measures branch and status only.
`status` exceeds 10x by the median in these cases. Other commands do not show the same improvement.

### Limits

- The host runs other workloads. The cache is warm or mixed; the test does not purge it.
- The 100,000-file candidate clean times are 2.224, 15.091, and 2.622 seconds. The variation is substantial.
- The first run reaches ENOSPC during the third baseline large-file add. File-count cases finish before that failure.
- The failed large-file run is retained in `paired-results.json`. A separate retry measures only the large-file case.
- These tests do not establish Linux behavior or performance.

## Large-file retry

The fixture contains 1,000 small files and one 1 GiB file. Each trial changes 4 KiB in the large file.
All three trials per binary complete, including staged status, commit, `fsck`, and checks of both file versions.

| Measurement | Before, median | After, median |
| --- | ---: | ---: |
| `add`, seconds | 3.537 | 1.791 |
| Extra allocation, MiB | 1024.461 | 0.398 |

The command is about 2x faster by the median. The clone avoids the extra 1 GiB allocation.
Host load varies during the retry. These figures do not establish a fixed speed ratio.
Raw results: [paired-large-results.json](paired-large-results.json).

## Correctness checks

- All 34 unit tests pass on the dedicated APFS volume before the destination-flag guard.
- The test for hard links, failure cleanup, working inode preservation, and the new destination-flag guard passes after that change.
- Nine status comparisons match the previous binary, including restored time, atomic replacement, staged changes, and active `mmap`.
- The APFS command suite passes 26 checks.
- The basic command suite passes 67 checks. The merge suite passes 47 checks.
- The basic and merge suites use GNU `sed` and `wc`. The first basic run with macOS tools fails during fixture preparation.
- The release build and `git diff --check` pass.

## Reproduce

```sh
python3 benchmarks/apfs-20261021/paired.py \
  --root /private/tmp/layr-apfs-paired-new \
  --baseline /path/to/old/layr \
  --candidate target/release/layr
```

Use a new root for each run. The script creates and removes its own APFS images.
Use `--cases large` to repeat only the 1 GiB case.

## Evidence

- Base commit: `dea82057fe6312a1365f9c34338c994bc48acc33`.
- Source file hashes at the time of measurement: [optimization-source-sha256.json](optimization-source-sha256.json).
- Raw paired results: [paired-results.json](paired-results.json).
- Script: [paired.py](paired.py).
- Status regression: [apfs-status.py](../../tests/apfs-status.py).
- Baseline SHA-256: `88602f0351b2d8d2bc4eb119369728f1e0ef6800e6cecc28c077ff2bdad04c18`.
- Candidate SHA-256: `40ceb6d727074f1d14a769a988eac889fd046d25d2acfac5c62aa48536a02a12`.

The kernel attribute contract is documented in Apple’s [getattrlist manual source](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/man/man2/getattrlist.2).
The implementation treats zero or unavailable generation counts as invalid.
