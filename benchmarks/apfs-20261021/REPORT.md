# APFS measurements, 2026-10-21

Source commit: `dea82057fe6312a1365f9c34338c994bc48acc33`.
Binary SHA-256: `88602f0351b2d8d2bc4eb119369728f1e0ef6800e6cecc28c077ff2bdad04c18`.

## Conditions

- Native macOS, release build, one benchmark process at a time.
- Separate 8 GiB case-sensitive APFS sparse image on the host Data volume.
- Host free space at start: 20.01 GiB. The host volume was 99% full.
- Other host work remained active. No cache purge. These are warm or mixed-cache results.
- Three trials unless stated otherwise. Tables show medians, not tail latency.
- Timed operations exclude sync and content verification. Allocation samples follow sync.
- These measurements do not prove an APFS performance limit.

## Complete Layr commands

Each trial starts from a new project. Ten of the small files change before diff, add, and commit.
All trials check clean status after merge and run fsck.

| Command | 1,000 files, median s | 10,000 files, median s | 100,000 files, one trial s |
|---|---:|---:|---:|
| init | 1.202 | 5.790 | 69.912 |
| branch | 0.183 | 1.331 | 21.709 |
| status_clean | 0.785 | 4.718 | 84.328 |
| diff_changed | 0.583 | 3.439 | 64.055 |
| add | 0.742 | 4.180 | 119.526 |
| commit | 0.474 | 2.621 | 58.482 |
| merge | 0.993 | 8.873 | 129.090 |
| fsck | 0.015 | 0.013 | 0.012 |

The 100,000-file case has only one trial. Do not treat it as a stable median.
At 10,000 files, clean status ranges from 3.151 to 5.800 seconds.

## Primitive operations

The C helper checks that both scan methods return the same entry count.
The bulk scan requests names, type, identity, times, owner, group, access mode, and data length.
It is not a complete replacement for Layr scan: markers, errors, and identity semantics need integration checks.

| Operation | 1,000 files, median s | 10,000 files, median s | 100,000 files, median s |
|---|---:|---:|---:|
| stat | 0.003672 | 0.036089 | 0.596803 |
| bulk | 0.002172 | 0.019794 | 0.225808 |
| clone | 0.011404 | 0.272716 | 3.233576 |
| xattr | 0.030439 | 0.417959 | 10.039211 |
| seal | 0.024579 | 0.371587 | 13.217183 |
| delete_tree | 0.068083 | 0.631966 | 9.583449 |

The helper performs two unseal passes per trial. The first writes zero flags to an already writable clone.
The second removes immutable flags after seal. Keep these measurements separate.

| Unseal phase | 1,000 files, median s | 10,000 files, median s | 100,000 files, median s |
|---|---:|---:|---:|
| Already writable | 0.044551 | 0.455934 | 4.822221 |
| Previously sealed | 0.023445 | 0.374390 | 13.010043 |

The xattr test writes a synthetic identity. It does not include Layr source-identity reads or safe path resolution.
The clone test does not include source validation, origin attributes, layer exclusion, or immutable flags.
The delete timer starts after unseal. It excludes the cost to remove immutable flags.

## Complete-command trace, 100,000 files

| Command | Whole command s | Snapshot totals s | Candidate comparison s | Other/unattributed s |
|---|---:|---:|---:|
| branch | 21.709 | 21.670 | 0.000 | 0.039 |
| status | 84.328 | 29.128 | 36.181 | 19.019 |
| add | 119.526 | 62.045 | 32.966 | 24.515 |
| commit | 58.482 | 26.642 | 25.619 | 6.220 |

Snapshot totals include their nested scan, copy, and seal spans. Do not add those nested spans again.
The remaining time is not fully instrumented. It includes work outside these spans; it is not a measured delete time.
`apfs::candidates` reads file pairs through `changed_ranges`, including unchanged files.

## Replace a file after a 4 KiB change

Three trials per size. Each trial preserves the old version through a clone.
Both methods must produce the same SHA-256 as the modified source.

| File size | Byte copy, median s | New clone plus rename, median s | Byte-copy allocation delta | Clone allocation delta |
|---|---:|---:|---:|---:|
| 64 MiB | 0.082819 | 0.005300 | 64.00 MiB | 0.00 MiB |
| 1024 MiB | 1.735534 | 0.005967 | 1025.00 MiB | 0.00 MiB |

This is a primitive comparison, not an optimized Layr add implementation.
Byte copy uses Python buffered I/O. Clone timing includes a helper process and rename.
The clone changes the destination inode. Hard links and open handles need a separate product design.
Zero allocation delta means no increase in this container sample; it does not prove zero metadata cost.

## Sparse file

One trial, 1 GiB logical size, four bytes at each end. SHA-256 matches after each operation.

| Method | Seconds | Container delta MiB | Reported blocks (512 bytes each) |
|---|---:|---:|---:|
| sparse_byte_copy | 1.002974 | 1024.00 | 2097152 |
| sparse_clone | 0.007555 | 0.00 | 64 |

## FSEvents and volume snapshots

- FSEvents delivered the target file event in 3.04 to 12.34 ms across three trials.
- Requested stream latency: 50 ms, with FileEvents and NoDefer.
- This tests one event per stream. It does not test loss, restart recovery, or an incremental status implementation.
- `fs_snapshot_create` returned EPERM (errno 1) under the current user.
- No snapshot was created. Volume-snapshot performance remains unmeasured.

## Change density

10,000 files. Three trials per changed-file count. Each trial uses a fresh branch.
Every dirty case checks the status path count, commits, runs fsck, and checks clean status.
Cases run in increasing change count, not randomized order. Host load varies.

| Changed files | Status, median s | Diff, median s | Add, median s | Commit, median s |
|---|---:|---:|---:|---:|
| 0 | 2.996 | 2.704 | Not run | Not run |
| 10 | 3.731 | 3.417 | 3.884 | 2.412 |
| 1000 | 2.824 | 2.963 | 4.064 | 2.387 |
| 10000 | 5.021 | 3.367 | 5.418 | 2.538 |

The clean case still takes about three seconds. Changing 1,000 files does not multiply the cost by 100 compared with ten files.
This is consistent with substantial whole-tree work. It does not quantify the benefit of an unimplemented incremental index.

## Actual Layr add for large files

Each source has 1,000 small files and one large file. Only 4 KiB of the large file changes.
Three fresh branches per size. All commits pass fsck and end with clean status.
The unchanged main file retains its original bytes at the modified offset.

| Large file | Branch, median s | Add, median s | Commit, median s | Container increase at add, median MiB |
|---|---:|---:|---:|---:|
| 64 MiB | 0.119 | 0.609 | 0.342 | 64.469 |
| 1024 MiB | 0.118 | 1.576 | 0.685 | 1024.469 |

These complete-command timings and the primitive copy timings come from separate fixtures and runs.
Do not subtract one from the other to predict an optimized add time.

## Hard-link failure

The hard-link workload stopped on its first branch check. The assertion was not removed.
Inspection of the retained image shows that the link is already lost after init.

| Stage | Alias inode | Original inode | Link counts | Equal bytes |
|---|---:|---:|---|---|
| source | 18 | 18 | [2, 2] | True |
| main | 30343 | 35772 | [1, 1] | True |
| branch | 40447 | 45876 | [1, 1] | True |

Reproduction: create `alias` with `os.link` to a source file, then run `layr init work --from SOURCE`.
Compare the two inode numbers under `store/projects/work/lines/main`.
Impact: writes through one imported name do not update the other name. File bytes survive import.
The `copy_folder` loop calls `fsutil::copy_entry` for each file separately. It does not maintain an inode-to-path map.
The branch measurement of 1.054 seconds is not a hard-link fallback measurement: main already contains separate files.
No validated workaround was established. This task records the defect; product code was not changed.

## Decisions supported by these measurements

1. Prioritize the repeated whole-tree snapshots and content comparisons. They dominate the observed large-tree commands.
2. Avoid complete byte copies when replacing large internal index files, after defining inode and hard-link semantics.
3. Keep sparse extents when a file can use the clone path. The byte-copy fallback expands this fixture by 1 GiB.
4. Treat bulk metadata reads as a smaller optimization. They improve scan time, but do not remove snapshot or comparison costs.
5. Investigate an incremental index with FSEvents. This run proves event delivery only, not correctness or command speed.
6. Fix import hard-link preservation before drawing conclusions about the hard-link snapshot fallback.

No complete Layr command was changed or accelerated in this task.
A tenfold improvement of all APFS commands remains unproven.

## Coverage limits

- Volume-snapshot performance is blocked by EPERM under the current user.
- Hard-link fallback timing is blocked by the import behavior described above.
- FSEvents overflow, restart recovery, and live concurrent writers were not performance-tested.
- Nested-layer fallback, network transfer, cold-cache behavior, and a less-full physical disk were not measured.
- The 100,000-file complete workload and sparse-file comparison each have one trial.
- Full command traces and primitive timings have different overhead and cannot be added as a speedup prediction.

## Reproduce

Run from the repository root. Use new output directories.

```sh
cargo build --release
xcrun clang -O2 -Wall -Wno-deprecated-declarations benchmarks/apfs-20261021/primitives.c -framework CoreServices -o /tmp/layr-apfs-primitives
python3 benchmarks/apfs-20261021/run.py --root /tmp/layr-apfs-measure-new --binary target/release/layr --helper /tmp/layr-apfs-primitives
python3 benchmarks/apfs-20261021/workloads.py --root /tmp/layr-apfs-density-new --binary target/release/layr --cases density
python3 benchmarks/apfs-20261021/workloads.py --root /tmp/layr-apfs-large-new --binary target/release/layr --cases large
# This command reproduces the hard-link assertion failure.
python3 benchmarks/apfs-20261021/workloads.py --root /tmp/layr-apfs-hardlink-new --binary target/release/layr --cases hardlink
```

The scripts attach only their own disk images and detach them in a finally block.
The first script retains its image for inspection. The workload script deletes each image after its successful case.
The scripts create output paths with exist_ok=False to prevent accidental reuse.

## Raw evidence

- `results.json`: primitives, allocation, capability probes, environment.
- `cli-1000.json`, `cli-10000.json`, `cli-100000.json`: complete command results and trace output.
- `workloads-initial.json`: completed density cases and the failed hard-link case.
- `workloads-large.json`: completed large-file cases.
- `hardlink-failure.json`: inode evidence from the retained image.
- `workloads-initial.py`: exact initial workload script.
- `workloads.py`: same workload with a case selector for independent completion.
- `environment.json`: hardware, OS version, and script hashes.
