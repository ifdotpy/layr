# Virtual-line Git command measurements

This page records the initial debug-build experiment at `e3e984f`.
The follow-up for `68ebb34` is in [release-paired.md](release-paired.md).

The source base is `cbd9b0e`. The test uses macOS FSKit mounts and object store v2.
Both binaries use the Rust debug profile and the same toolchain (1.99.1).
The machine is shared with other work. These measurements do not predict release-build latency.
The JSON files include each sample and the binary SHA-256 hashes.

## Method

`run.py` creates two virtual lines from one state with 1,000 files.
Each trial changes the same five files and measures complete CLI commands.
The order alternates between the old and new binaries.
There are nine trials before Git export and nine after the first export.
The first export includes the initial state and nine saved changes.
Later exports each add one saved change.
Every save must leave a clean status. No `.git` appears in either mount.

The original run also started a 10,000-file case. It was stopped after the 1,000-file results
showed different read times between mounts. No incomplete 10,000-file result is reported.

`same-mount.py` checks that variation with both binaries on the same virtual line.
It uses the line that already has a Git export. Each of nine pairs changes five files.
It measures `status`, `diff`, and `save`, with alternating binary order.
Neither binary exports new commits during those saves.

These tests measure local command latency. They do not measure network transfers,
Linux performance, or merge performance. At this experiment's revision, `layr git pull` refuses virtual lines.

## Reproduce

~~~sh
python3 benchmarks/virtual-git/run.py \
  --before /path/to/before/layr --after /path/to/after/layr \
  --files 1000 --rounds 9 --out /tmp/virtual-git-results.json
python3 benchmarks/virtual-git/same-mount.py \
  --before /path/to/before/layr --after /path/to/after/layr \
  --line <work-from-results>/store/projects/bench-1000/lines/after \
  --rounds 9 --out /tmp/same-mount-results.json
~~~

Use a normal macOS user. The LayrFS extension must be active.
The scripts keep their private stores for inspection and hide completed mounts.

## Results

Same mount, median milliseconds per complete command:

| Command | Before | After | Difference |
| --- | ---: | ---: | ---: |
| `status` | 1688.2 | 1749.4 | +3.6% |
| `diff` | 1642.3 | 1782.5 | +8.5% |
| `save` | 1751.0 | 1903.9 | +8.7% |

The paired samples vary substantially. The save ratios range from -45% to +60%.
These measurements do not establish a tight upper bound on regression.
The source change adds no Git work to save, status, or diff.
The integration test also verifies that a macOS virtual save creates no Git commit ref.

The first local export of ten states took 22.38 seconds.
The median later export took 2.19 seconds per changed state.
These export costs apply to an explicit `layr git sync`, not to save.

## Validation

- `tests/views-git.sh`: macOS FSKit and object store v2.
- `tests/t03-git.sh`: 70 checks passed.
- `tests/t08-gitcompat.sh`: 28 checks passed.
- `guard-test.log`: the old binary refuses Git on a virtual line; the new binary accepts it.
- Linux execution remains for CI.

`source.patch` records the source used by the benchmark binaries against the base state.
The final source removes only an optional trailing comma from that patch.
