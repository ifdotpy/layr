# Performance

This page is part of the [Layr documentation](../README.md#documentation).

## Disk use compared with Git worktrees

**16 branches with private caches. About 94% less disk space.**
Layr on btrfs: **153 MiB**. Git worktrees on btrfs with full cache copies: **2508 MiB**.
These totals include the main workspace. The measured fixture uses 128 MiB of cache data per workspace.

Two runs use the same synthetic project: 2,048 source files, one main workspace, and 16 branches with one edit and commit each.
The cache case adds 128 MiB of private mutable data per workspace. Totals include history and additional filesystem metadata.

| Scenario | Layr | Git worktree |
| --- | ---: | ---: |
| Source only, btrfs | 22 MiB | 325 MiB |
| Source only, OverlayFS on ext4 versus Git on ext4 | 33 MiB | 283 MiB |
| Source only, OverlayFS on XFS versus Git on XFS | 19 MiB | 301 MiB |
| Private caches, btrfs, full Git cache copies | 153 MiB | 2508 MiB |
| Private caches, btrfs, reflink Git cache copies | 153 MiB | 455 MiB |

Layr on btrfs uses about 94% less space than Git with full cache copies in this fixture.
With reflink cache copies, the reduction is about 66%.
Git worktree does not populate ignored caches itself; the benchmark copies them explicitly.
Shared package-manager caches can change the result.

The current OverlayFS cache path copies data even on XFS: 2324 MiB versus 432 MiB for Git with reflink cache copies.
The source-only result does not establish the same benefit for private caches.
See the [method, full comparison and both raw runs](../benchmarks/workspace-disk-20261016/README.md).

## Command latency

One run in a Linux VM (OrbStack, 17 vCPU, kernel 7.0), release build, btrfs on a loop file.
Project: 200,000 small files and one 100 MiB file. The second column follows a write of 300 MiB
of ignored build output, so btrfs is still flushing; the third is a quiet tree.

| Operation                                | after a large write | quiet  |
| ---------------------------------------- | ------------------: | -----: |
| `status`, clean                          | 0.02 s              | 0.02 s |
| `status`, 101 edits                      | 0.80 s              | 0.05 s |
| `diff --stat`                            | 0.24 s              |        |
| `add -A`                                 | 0.68 s              |        |
| `commit`                                 | 1.48 s              | 0.35 to 0.5 s |
| `branch` (a new line, any size)          | 0.18 s              | 0.28 s |
| `merge` (50 + 50 changed files)          | 1.35 s              |        |
| `log -5`, `show --stat`                  | 0.003 s, 0.014 s    |        |
| `git status` of the same tree, for scale | 0.14 s              |        |

Most of the time is btrfs transaction commits: every snapshot commits one (15 to 600 ms, more
while dirty data is being written). The database lives in its own subvolume: an fsync there
after a snapshot takes about 6 ms, against about 100 ms in the subvolume that holds the
snapshot folders.

### APFS comparison (provisional)

Measured on 2026-10-14 with the implementation in `ada0cef`.
Each fixture contains 1,000 or 10,000 files of 4 KiB. Ten files receive an extra line.
The source content hashes match across both backends. Each operation has three trials.
The table gives median seconds, including command startup.

APFS runs natively on macOS 27.2, on a case-sensitive volume in an ASIF image.
btrfs runs in the OrbStack Linux VM on the same Mac.
Those initial backend runs were sequential. The host also ran other tasks. Their load was not controlled.

| Operation | btrfs, 1,000 files | APFS, 1,000 files | btrfs, 10,000 files | APFS, 10,000 files |
| --- | ---: | ---: | ---: | ---: |
| init | 0.354 | 1.040 | 0.509 | 18.289 |
| branch | 0.035 | 0.285 | 0.032 | 3.618 |
| status, clean | 0.016 | 0.674 | 0.014 | 7.758 |
| diff, 10 edits | 0.021 | 0.627 | 0.019 | 7.766 |
| add -A | 0.028 | 0.897 | 0.024 | 12.314 |
| commit | 0.030 | 0.534 | 0.025 | 8.849 |
| merge, fast-forward | 0.043 | 1.350 | 0.040 | 35.101 |

These values are provisional. They do not establish an isolated filesystem speed ratio.
A later test alternates clean status commands on both backends with 10,000 files.
It measures APFS at 11.00 to 13.26 seconds and btrfs at 0.09 to 16.33 seconds.
The host's one-minute load average ranges from 294 to 311 during that test.
A further benchmark found no quiet interval within 30 minutes. The comparison remains provisional.

The measured baseline clones every file for a snapshot and scans content for changed blocks.
The btrfs implementation uses subvolume snapshots and send-stream change lists.
Those mechanisms have different costs as the file count grows.
The test image still has 96% free space after the runs. Spotlight indexing is disabled on it.

Raw data includes the binary hash and source content hash:
[APFS, 1,000 files](../benchmarks/results/apfs-1000.json),
[APFS, 10,000 files](../benchmarks/results/apfs-10000.json),
[btrfs, 1,000 files](../benchmarks/results/btrfs-1000.json),
[btrfs, 10,000 files](../benchmarks/results/btrfs-10000.json),
and [interleaved status](../benchmarks/results/paired-status-10000.json).

At the benchmark revision, the previous btrfs binary (`c6063e4`) and the candidate passed thirteen compatibility checks.
Both binaries read each other's states and verify signatures without changing state identifiers.
Run those checks with `OLD_LAYR=/path/to/old/layr LAYR=/path/to/new/layr bash tests/compat-btrfs.sh` on Linux as root.

A later APFS run includes overlays and the compatible transfer implementation.
Three trials give these median times:

| Operation | 1,000 files | 10,000 files |
| --- | ---: | ---: |
| branch | 0.388 s | 4.058 s |
| status, clean | 1.074 s | 9.264 s |
| commit | 0.850 s | 8.666 s |
| merge, fast-forward | 2.401 s | 22.721 s |

The host had other work, so these results remain provisional.
The Linux comparison could not start because the OrbStack executable was absent.
These results do not establish a speed ratio against btrfs.
The measured binary predates the final service permission checks.
Raw data: [1,000 files](../benchmarks/results/apfs-complete-1000.json)
and [10,000 files](../benchmarks/results/apfs-complete-10000.json).

