# OverlayFS delta snapshots, 2026-10-16

Compare the full-tree backend at `f1f9a94` with the delta backend at `6dee8f6`.
Run both release binaries in the same Linux VM, on the same local loop filesystem.
Each binary gets a new store. The fixture has 10,000 files, up to 4 KiB each, and changes 10 files.
Each result contains three trials, binary hashes, fixture hashes, wall time, CPU time and load.
The reported command times are medians. The benchmark runs as root without the service.
It does not measure a branch whose owner changes.

Reproduce on an isolated ext4 or XFS filesystem with mount permission:

```sh
python3 benchmarks/overlayfs-delta.py --before /path/to/before --after /path/to/after --root /testfs/new-benchmark
```

Private file allocation uses `st_blocks`, with hard links counted once.
It excludes directories, filesystem metadata, the source fixture and visible mount views.
On ext4, this measures allocated file blocks. On XFS, shared reflink extents can be counted more than once.
Do not treat the XFS allocation count as physical device usage.

The backend still scans metadata. It copies the live upper layer, which can contain changes from several commits.
It uses a full copy for hard-linked trees and when it combines a long layer chain.

## ext4 results

The test image has 16 GiB. Times are milliseconds, with three trials per binary.

| Command | Before | After | Before / after |
| --- | ---: | ---: | ---: |
| init | 2201.0 | 1970.9 | 1.12 |
| branch | 102.9 | 113.4 | 0.91 |
| clean status | 1068.8 | 395.4 | 2.70 |
| diff | 1095.0 | 276.3 | 3.96 |
| add | 1534.9 | 415.9 | 3.69 |
| commit | 944.6 | 438.1 | 2.16 |
| merge | 3144.9 | 866.8 | 3.63 |

Private regular-file allocation falls from 492,957,696 to 247,443,456 bytes (49.8% less).
This covers the three benchmark projects. It is not the space for one commit.
The regular-file count falls from 120,063 to 60,123.
The first attempt exhausted inodes on a 4 GiB ext4 image. Those incomplete results are excluded.

## XFS first run

The XFS image has 4 GiB. The sequential run has a large load change during the new binary's trials.
Median commit time rises from 840.2 to 1199.7 ms. Median branch time rises from 53.1 to 3470.1 ms.
Median branch CPU time is 9 ms before and 17 ms after. The wall-time increase needs a separate comparison.
Keep these raw results in `xfs.json`; do not use them to claim an XFS speed improvement.

## XFS alternating comparison

Run order: before, after, after, before, before, after. Each run uses a new store with one benchmark project.
The fixture and binaries match the first comparison. Raw results are in `xfs-interleaved.json`.
Times are milliseconds, with three trials per binary.

| Command | Before | After | Before / after |
| --- | ---: | ---: | ---: |
| init | 10343.0 | 19113.3 | 0.54 |
| branch | 1963.2 | 787.8 | 2.49 |
| clean status | 4659.3 | 4053.5 | 1.15 |
| diff | 3986.2 | 2827.4 | 1.41 |
| add | 4166.4 | 2874.9 | 1.45 |
| commit | 4675.4 | 2125.3 | 2.20 |
| merge | 13279.1 | 3939.2 | 3.37 |

Commit and merge improve in all three alternating pairs. However, `init` is slower in two pairs and its median rises by 85%.
The VM has large wall-time variation. These observations do not establish stable XFS latency or explain the init regression.
Do not claim that every operation becomes faster, or compare these absolute times with the earlier ext4 run.

## Verification

- ext4 and XFS: 27 unit tests each, including cache path replacement, pinned destination directories, open writers and layer compaction.
- ext4 and XFS: 12 cache checks each. Service: 28 checks.
- Integration: basic, merge, Git, transfer, sync, permissions, access and 151 overlay checks passed.
- btrfs: transfer, sync, audit, 11 storage checks and 20 compatibility checks passed.
- Backup transfer from OverlayFS to btrfs and back preserves identifiers, content, modes, links and signatures.
- APFS: 30 tests passed.
- A store created by the previous OverlayFS binary opens with the new binary. Its cache mount becomes an ordinary directory.
- The previous binary rejects the new registry instead of silently ignoring layers.

The broad integration run precedes the final cache path guards. The final guards have unit, cache and service coverage.
No remote CI run, push or merge was performed for this change.
