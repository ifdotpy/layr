# The OverlayFS backend (ext4 and XFS)

This page is part of the [Layr documentation](../README.md#documentation).

The Linux `overlayfs` feature adds a backend for a VM with ext4 or XFS and root mount permission.
See [the setup, transfer rules and limits](overlayfs.md).
The default Linux build remains btrfs.
Working caches are ordinary directories, so tools can remove and recreate them with `rm -rf`.
Snapshots copy the upper layer and share unchanged lower layers.

### OverlayFS benchmark results

Measured on 2026-10-16 with release binaries in one Linux VM: 10,000 files, 10 edits, three trials per binary.
The comparison uses the previous full-tree implementation and the new delta snapshots. Times are medians.
Commands run as root without the service; these tests do not measure changes of branch ownership.

| Filesystem | Operation | Full-tree snapshots | Delta snapshots | Speedup |
| --- | --- | ---: | ---: | ---: |
| ext4 | Clean status | 1069 ms | 395 ms | 2.7x |
| ext4 | Diff | 1095 ms | 276 ms | 4.0x |
| ext4 | Commit | 945 ms | 438 ms | 2.2x |
| ext4 | Merge | 3145 ms | 867 ms | 3.6x |
| XFS | Commit | 4675 ms | 2125 ms | 2.2x |
| XFS | Merge | 13279 ms | 3939 ms | 3.4x |

- ext4 private file allocation falls by 49.8%, from 492,957,696 to 247,443,456 bytes across three benchmark projects.
  This excludes directory and filesystem metadata. It is not the cost of one commit.
- XFS uses alternating runs. Its median `init` time rises from 10.3 to 19.1 seconds, an unresolved regression.
- VM latency varies greatly. Do not compare absolute ext4 and XFS times or treat these results as fixed backend limits.
- XFS can share reflink extents. Its file block counts do not establish physical space savings.
- Hard-linked trees and layer compaction still require full copies. The upper layer can contain changes from several commits.

See the [full report and raw samples](../benchmarks/overlayfs-delta-20261016/README.md) for commands, hashes, all results and test coverage.
