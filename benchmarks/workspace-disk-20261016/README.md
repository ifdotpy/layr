# Workspace disk use, 2026-10-16

## Results

Two complete runs give the ranges below. Values are MiB (1 MiB = 1,048,576 bytes).
Each total includes one main workspace and 16 additional branches, with one commit per branch.

| Backend | Source only | With a private 128 MiB cache per workspace |
| --- | ---: | ---: |
| Layr btrfs | 22.2 to 22.3 | 152.9 to 153.0 |
| Layr OverlayFS on ext4 | 33.4 | 2337.5 |
| Layr OverlayFS on XFS | 18.6 | 2323.8 |
| Git worktree on btrfs, full cache copies | 324.3 to 324.7 | 2507.5 to 2507.8 |
| Git worktree on ext4, full cache copies | 283.3 | 2459.4 |
| Git worktree on XFS, full cache copies | 300.5 | 2477.7 |
| Git worktree on btrfs, reflink cache copies | 324.4 to 325.2 | 455.2 to 455.8 |
| Git worktree on XFS, reflink cache copies | 300.5 | 431.7 |

Both runs pass all content-isolation checks and each repository's `fsck`.
Raw results: [first run](results.json), [repeat](repeat.json).

## Claims supported by this fixture

- With private caches, Layr btrfs uses about 94% less space than Git worktrees with full cache copies on btrfs.
  The totals are 153 MiB and 2508 MiB for main plus 16 branches.
- With source only, Layr OverlayFS on XFS uses about 94% less space than Git worktrees on XFS: 18.6 versus 300.5 MiB.
- Against Git with reflink cache copies on btrfs, Layr uses about 66% less space: 153 versus 455 MiB.

Use the workload and cache-copy condition with each claim. These are measured synthetic cases, not a universal Git comparison.
The current OverlayFS warm-cache result is worse than Git with reflink cache copies on XFS: 2324 versus 432 MiB.
OverlayFS on ext4 saves about 5% in the warm-cache case, compared with Git on ext4.
The strongest cache result belongs to the btrfs backend.

## Workload

The fixture contains 2,048 text files of 8 KiB each, plus a cache ignore rule.
One main workspace produces 16 independent branches. Each branch changes one source file and commits it.
The warm-cache case adds 128 MiB of deterministic binary data to each workspace.
Each branch changes 4 KiB of one cache file. Checks confirm that the main workspace keeps its original content.

The cache represents private mutable dependencies or build output. This is synthetic data, not an npm or Cargo installation.
A package manager with a shared cache, hard links, or a different dependency layout can use less space.
Git worktree does not populate ignored caches itself. This benchmark explicitly copies the main cache into each Git worktree.

## Method

- Each tool and filesystem pair uses a new 8 GiB loop filesystem in the same isolated Linux container.
- No Mac directories are mounted into the container.
- The source-only and warm-cache cases use separate projects. Each case subtracts filesystem usage before its project exists.
- The totals include the main workspace, 16 branches, their histories, and additional filesystem metadata.
- Layr also includes its saved cache and automatic states. No history or cache cleanup runs during the measurement.
- Git packs its initial object database with `git gc` before the main measurement. Branches share that database.
- `sync` runs before each sample. btrfs uses `btrfs filesystem usage -b`, field `Used`, including metadata redundancy.
- ext4 and XFS use allocated filesystem blocks from `statvfs`, not the sum of per-file `st_blocks`.
- This measures used space inside the guest filesystem. It does not measure host image allocation, SSD writes, or RAM.
- Fixture files and benchmark logs remain outside the measured filesystem. No filesystem compression is enabled.
- Raw results include binary hashes, source and cache hashes, commands, and samples after 1, 4, 8 and 16 branches.

## Reproduce

Run in an isolated Linux VM or privileged container with no host directory mounts:

```sh
python3 benchmarks/workspace-disk.py \
  --root /tmp/new-workspace-disk-test \
  --btrfs-binary /path/to/layr-btrfs \
  --overlay-binary /path/to/layr-overlayfs
```

The command requires Git, btrfs-progs, e2fsprogs and xfsprogs.
It retains each image and result. It unmounts only the filesystems it creates.
Use a private parent mount for OverlayFS, as described in [the backend guide](../../docs/overlayfs.md).

## Limits

The Git `never` cases copy cache bytes independently, as separate installations can do.
The Git `always` cases use `cp --reflink=always` for the cache. Source worktrees still use normal Git checkout.
Keep both comparisons visible. A comparison against full cache copies alone does not describe an optimized Git setup.

OverlayFS shares unchanged source layers well. In this run, its warm caches require separate data copies even on XFS.
Do not claim that the current OverlayFS cache path shares those XFS extents.
The btrfs backend shares both source snapshots and cache extents.
The test does not cover concurrent writes, different workspace owners, long histories, or large source changes.
