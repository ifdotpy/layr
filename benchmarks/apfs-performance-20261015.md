# APFS performance experiment, 2026-10-15

Base commit: `76623f1194df6b461a159baec6f23698068be644`. Branch: `experiment/apfs-performance`.

The fixture has 10,000 files. Each trial changes ten files. Each comparison uses two trials and rotates variant order.
The scripts record elapsed time, child CPU time, host load, binary hashes, and trace output.

## Full commands

Times are medians in seconds. Native mode includes source checks, origin attributes, and immutable flags.
The metadata variant uses scan results again and removes origin checks before exact byte comparison.
Version 2 also skips a second content check when the first comparison finds no changed bytes.

### Version 1

| Command | Baseline elapsed | Native elapsed | Baseline CPU | Native CPU |
|---|---:|---:|---:|---:|
| branch | 6.436 | 1.863 | 6.150 | 1.804 |
| status_clean | 10.532 | 5.176 | 9.470 | 4.833 |
| diff_changed | 10.825 | 5.289 | 9.965 | 4.912 |
| add | 14.703 | 6.827 | 14.129 | 6.399 |
| commit | 8.170 | 4.051 | 7.858 | 3.759 |

Host load (one minute): 24.1 to 59.4.

### Version 2

| Command | Baseline elapsed | Native elapsed | Baseline CPU | Native CPU |
|---|---:|---:|---:|---:|
| branch | 3.348 | 1.236 | 3.296 | 1.210 |
| status_clean | 5.718 | 3.110 | 5.572 | 2.899 |
| diff_changed | 6.425 | 3.015 | 6.153 | 2.806 |
| add | 12.179 | 5.382 | 11.599 | 4.462 |
| commit | 7.169 | 4.951 | 6.840 | 2.774 |

Host load (one minute): 9.3 to 127.9.

## Practical limit

The first comparison shows about 2x faster commands and 3.45x faster branch creation.
The second comparison also shows a benefit, but the changing host load prevents a precise claim about the version 2 change.
Metadata changes alone do not give a consistent speed benefit.

The primitive test has three trials on a separate 10,000-file fixture.
A whole-directory clone takes 0.108 seconds. A file-by-file clone takes 1.161 seconds with one worker and 0.525 seconds with eight workers.
A directory scan takes 0.045 seconds. A complete file read takes 0.254 seconds.
These primitive times exclude Layr origin attributes, layer exclusion, source checks, and immutable flags.
They are measured reference points, not a lower bound for a complete Layr command.

A complete branch takes 1.24 to 1.86 seconds in these experiments. Other commands still take several seconds.
Further gains require fewer attribute operations, fewer repeated scans, or fewer temporary snapshots.
No experiment here proves an absolute APFS limit.

## Compatibility limits

Native directory mode requires `LAYR_APFS_EXPERIMENT_CLONE_TREE=1`.
Trees with nested layer directories use the existing copy path.
The Darwin clonefile manual discourages recursive directory clones. This mode needs further compatibility work before production use.
Both version 1 modes pass 28 Rust tests. Version 2 native mode also passes 28 Rust tests.
Version 2 also completes `apfs-local.sh` and the basic, merge, Git, transfer, sync, and Git compatibility suites with exit code 0.
The monitor reports no retained output, so this report does not claim individual assertion counts.
The experiments do not change main.

## Reproduce

Use `benchmarks/backends.py` to create the common fixture.
Use `benchmarks/apfs-paired.py --help` for the comparison arguments.
Use `benchmarks/apfs-primitives.py --help` for the primitive arguments.
Raw results are in the three adjacent JSON files with the date `20261015`.
