# OverlayFS measurements, 2026-10-16

Release builds, one Linux VM. Each filesystem uses a separate 4 GiB loop image.
The VM stores these images on the same host filesystem. These results do not represent separate physical disks.

The fixture contains 1,000 or 10,000 files of 4 KiB. Each commit changes ten files.
The table shows medians of three runs, in milliseconds.
The command order was btrfs, ext4, XFS for each file count.
The later runs have more timing variation. Do not use these results to rank ext4 against XFS.

| Files | Backend | Init | Branch | Status | Diff | Add | Commit | Merge |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1000 | btrfs | 123.8 | 21.6 | 5.4 | 5.8 | 15.3 | 17.1 | 29.8 |
| 1000 | OverlayFS / ext4 | 237.4 | 32.8 | 80.6 | 83.6 | 113.5 | 80.8 | 235.6 |
| 1000 | OverlayFS / xfs | 192.6 | 20.2 | 66.2 | 63.4 | 84.8 | 84.5 | 181.0 |
| 10000 | btrfs | 328.0 | 23.3 | 7.6 | 7.5 | 17.1 | 18.4 | 40.0 |
| 10000 | OverlayFS / ext4 | 1062.8 | 34.2 | 498.5 | 570.7 | 752.1 | 737.8 | 2025.3 |
| 10000 | OverlayFS / xfs | 1303.3 | 52.9 | 777.8 | 789.2 | 981.6 | 634.1 | 1954.9 |

Branch creation shares immutable data. Status and commit still scan or copy the visible tree.
At 10,000 files, OverlayFS commits take about 0.6 to 0.7 seconds, compared with 0.018 seconds on btrfs.

The JSON files include binary hashes, fixture hashes, each trial, CPU time, system load and command traces.
Use `benchmarks/backends.py` with a fresh root to repeat the workload.

These measurements precede the final retry for metadata changes and removal of unused upper directories.
The binary hashes identify the measured candidates.
These measurements do not test crash recovery or disk space use.
