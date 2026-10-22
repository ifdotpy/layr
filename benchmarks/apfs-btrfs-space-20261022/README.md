# APFS and btrfs space use, 2026-10-22

Both backends use Layr commit `0713d25623a87b97d3a0aefb6c476205d478c3b2`.
Each case has two complete trials on a new filesystem.
All twelve trials pass content checks and fsck.
The fixture hashes match between backends.

## Total space

The totals include main, sixteen branches, and one commit per branch.
Values exclude the empty filesystem and the original fixture outside it.
They include file data, metadata, and Layr records. One MiB is 1,048,576 bytes.
The table shows the mean of two trials.

| Initial fixture | APFS, MiB | btrfs, MiB | APFS / btrfs |
| --- | ---: | ---: | ---: |
| 10,000 initially empty files | 123.10 | 11.46 | 10.74x |
| 10,000 files of 4 KiB | 178.47 | 55.14 | 3.24x |
| 1,000 files of 4 KiB and one file of 1 GiB | 1054.31 | 1035.04 | 1.02x |

For this Layr workload, APFS uses 3.24 times as much space with small files.
With the large file, the difference is 1.86 percent.
These ratios describe these fixtures and this Layr backend, not every use of either filesystem.

## Growth after main

This table excludes the original main workspace. It shows the added space for sixteen branches and their commits.

| Initial fixture | APFS growth, MiB | btrfs growth, MiB |
| --- | ---: | ---: |
| 10,000 initially empty files | 114.78 | 3.98 |
| 10,000 files of 4 KiB | 128.31 | 3.76 |
| 1,000 files of 4 KiB and one file of 1 GiB | 13.44 | 3.52 |

The large file stays shared. Sixteen branches and commits do not create sixteen full copies of its data.
Most of the difference in the small-file cases appears as the number of tree copies increases.
Layr uses directory clones on APFS and subvolume snapshots on btrfs.
Btrfs snapshots initially share their tree root and copy metadata as changes occur.
See the [btrfs design](https://btrfs.readthedocs.io/en/stable/dev/dev-btrfs-design.html).

## Stages

Each value is the mean of two trials, above the empty filesystem allocation.

| Fixture | Backend | Main, MiB | After 16 branches, MiB | After 16 commits, MiB |
| --- | --- | ---: | ---: | ---: |
| 10,000 initially empty files | apfs | 8.32 | 65.16 | 123.10 |
| 10,000 initially empty files | btrfs | 7.48 | 8.61 | 11.46 |
| 10,000 files of 4 KiB | apfs | 50.16 | 114.16 | 178.47 |
| 10,000 files of 4 KiB | btrfs | 51.39 | 52.52 | 55.14 |
| 1,000 files of 4 KiB and one file of 1 GiB | apfs | 1040.87 | 1047.10 | 1054.31 |
| 1,000 files of 4 KiB and one file of 1 GiB | btrfs | 1031.51 | 1032.58 | 1035.04 |

## Method

- Each trial uses a separate 8 GiB filesystem image.
- APFS runs on a case-sensitive volume on macOS. Spotlight is disabled for the image.
- Btrfs runs inside an isolated Linux container in OrbStack. The container has no host directory mounts.
- No filesystem compression is enabled. Btrfs retains its default metadata profile.
- The source fixture is outside the measured filesystem. Its content is identical on both platforms.
- All sixteen branches are created from main before any branch changes.
- Each branch changes ten files. Each change writes 4 KiB, or one byte in the initially empty-file case.
- The large-file case also changes 4 KiB at a separate offset in the large file for each branch.
- Each branch runs `add -A`, then `commit`. No garbage collection removes history.
- Each sample follows sync. Btrfs also runs filesystem sync.
- APFS allocation is container capacity minus free capacity. Btrfs allocation is `Used` from `btrfs filesystem usage -b`.
- Btrfs allocation includes metadata redundancy. It is not the size of allocated block groups.
- Each reported sample is the median of three reads, with a one-second interval.
- Results include samples after 1, 4, 8, and 16 branches and commits.

## Checks and limits

Every branch has clean status after commit. The edited file bytes match the expected content.
All main files remain unchanged. The large main file matches its full SHA-256 hash.
Every trial passes fsck. Both backends finish six trials and remove their test images.

The APFS command observer loses its callback result after the work completes.
The saved result confirms all six successful trials and completion. No test image remains.
The benchmark is not restarted.
The Linux build observer also loses its callback. The resulting binary runs and completes all six trials.

The APFS large-file total ranges from 1,052.61 to 1,056.01 MiB.
Its added space after main is 13.44 MiB in both trials.
The btrfs large-file total ranges from 1,035.00 to 1,035.07 MiB.
The summary file contains ranges for every case and phase.

This measures allocation inside each filesystem, including its metadata.
It does not measure host virtual disk allocation, SSD writes, RAM, or the total Docker installation.
It does not compare compression, real dependency caches, long linear histories, or large data rewrites.

## Reproduce

On macOS:

```sh
python3 benchmarks/apfs-btrfs-space-20261022/measure.py \
  --backend apfs --binary /path/to/layr \
  --root /private/tmp/new-apfs-space-test
```

Inside an isolated Linux VM or privileged container with no host directory mounts:

```sh
python3 measure.py --backend btrfs --binary /path/to/layr \
  --root /tmp/new-btrfs-space-test
```

The Linux run needs btrfs-progs and permission to mount its own loop images.
Both commands create and remove only their own images. The result files remain at the root path.

## Artifacts

- `apfs-results.json` and `btrfs-results.json`: samples, fixture hashes, versions, and validation results.
- `summary.json`: per-case means, ranges, and ratios. With two trials, the median equals the mean.
- `environment.json`: source commit, binary hashes, compiler, and container identity.
- `measure.py`: the common script for both backends.
