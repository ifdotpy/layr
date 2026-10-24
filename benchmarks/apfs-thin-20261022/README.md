# Thin APFS states, 2026-10-22

This report measures two changes against `67ab5a6` (main):

1. `b305e84`: the creation time replaces the `com.layr.origin` attribute as the file identity.
2. `5ea956c` to `58e1ff2`: thin states. See [the design](../../docs/apfs-thin-states.md).

## Cost of one tree copy on APFS

`clone-cost.py` copies a tree of 100 folders with 100 files each, four times.
The values are container allocation per copy, after sync.

| Method | Empty files, bytes per entry | 4 KiB files, bytes per entry |
| --- | ---: | ---: |
| Directory clone | 281 | 327 |
| Directory clone and a new 38-byte attribute on each entry | 486 | 577 |
| Directory clone of entries that already have the attribute | 360 | 403 |
| Clone of each file into new folders | 256 | 310 |
| Hard links into new folders | 252 | 204 |

- The attribute costs about 80 bytes per entry in every copy.
- Immutable flags and attribute rewrites cost no space. They cost CPU.
- Hard links are not usable for writable lines, and they are slow to make.

## Space: main, 16 branches, one commit per branch

Same script and fixtures as [the earlier comparison](../apfs-btrfs-space-20261022/README.md).
Two trials per case. Values are MiB above the empty filesystem.

| Fixture | APFS `67ab5a6` | APFS `b305e84` | APFS thin | btrfs |
| --- | ---: | ---: | ---: | ---: |
| 10,000 empty files | 123.10 | 94.0 | 50.01 | 11.46 |
| 10,000 files of 4 KiB | 178.47 | 151.5 | 98.27 | 55.14 |
| 1,000 files of 4 KiB and one file of 1 GiB | 1054.31 | 1041.9 | 1048.02 | 1035.04 |

| Fixture | APFS / btrfs before | APFS / btrfs now |
| --- | ---: | ---: |
| 10,000 empty files | 10.74x | 4.36x |
| 10,000 files of 4 KiB | 3.24x | 1.78x |
| 1,000 files of 4 KiB and one file of 1 GiB | 1.02x | 1.01x |

The 16 commits add 0.96 MiB and 0.73 MiB on APFS now. On btrfs they add 2.85 MiB and 2.62 MiB.
The remaining difference comes from the 16 line folders: each is one tree copy (3.16 MiB for 10,000 files of 4 KiB).

The `b305e84` run has five complete trials. Its last large-file trial stopped at the 30-minute limit.
Its values are means of the complete trials (`space-creation-time-results.json`).

The large-file `main` value differs between runs (1030.3 to 1041.9 MiB on APFS), so its totals are not comparable.
The growth after `main` (16 branches and 16 commits) is comparable:

| Fixture | APFS `67ab5a6` | APFS `b305e84` | APFS thin | btrfs |
| --- | ---: | ---: | ---: | ---: |
| 10,000 empty files | 114.78 | 88.4 | 44.37 | 3.98 |
| 10,000 files of 4 KiB | 128.31 | 104.5 | 51.29 | 3.76 |
| 1,000 files of 4 KiB and one file of 1 GiB | 13.44 | 11.6 | 6.09 | 3.52 |

## CPU: one line, 10,000 files of 4 KiB

`paired.py` runs both binaries on the same APFS volume in alternate order, four trials.
CPU is user and system time of the command. Values are medians.
The host ran other work (load average above 150), so wall times are not reported.

| Command | CPU before, s | CPU after, s | Space before, MiB | Space after, MiB |
| --- | ---: | ---: | ---: | ---: |
| `init` | 9.60 | 7.75 | 9.72 | 6.53 |
| `branch` | 2.06 | 1.01 | 3.89 | 3.16 |
| `save` (automatic state, 10 changed files) | 2.26 | 0.52 | 3.91 | 0.07 |
| `add -A` | 2.58 | 0.38 | 3.90 | 0.01 |
| `commit` | 0.90 | 0.21 | 0 | 0 |
| `status` | 0.60 | 0.38 | | |
| `branch` from a thin head | 1.96 | 0.94 | 4.65 | 3.17 |

`paired-results.json` uses `865b915`. `paired-final-results.json` repeats the run with `2622d34`.
Its CPU values agree within 0.08 s. Its space values are not valid:
other tests on the same volume freed space during that run.

`diff.py` measures `layr diff --stat` against the working folder with 10 changed files, three runs each.
CPU: 3.75 s before, 0.33 s after (`58e1ff2`). Both outputs are equal.

## Layr lines and git worktrees

`worktree.py` compares 4 git worktrees with 4 Layr lines on one APFS volume.
Results: `worktree-small.json` (10,000 files of 4 KiB) and `worktree-large.json` (plus one file of 1 GiB).
See [the comparison](../../docs/apfs-vs-btrfs.md#layr-lines-and-git-worktrees-on-apfs).

## Checks

- Unit tests: 45 pass on a case-sensitive APFS volume.
- `tests/apfs-thin.py`, `tests/apfs-index.py`, `tests/apfs-local.sh`: pass.
- `tests/apfs-add.py` and `tests/apfs-status.py` against `67ab5a6`: pass. Each binary uses its own store.
- `t01`, `t02`, `t03`, `t05`, `t06`, `t08`: pass on macOS.
- The Linux btrfs and OverlayFS builds compile with no new warnings.

## Reproduce

```sh
python3 benchmarks/apfs-thin-20261022/paired.py OLD_LAYR NEW_LAYR /Volumes/CaseSensitive/new-folder 4
python3 benchmarks/apfs-thin-20261022/diff.py OLD_LAYR NEW_LAYR /Volumes/CaseSensitive/new-folder
python3 benchmarks/apfs-btrfs-space-20261022/measure.py --backend apfs --binary NEW_LAYR --root /private/tmp/new-folder
python3 benchmarks/apfs-thin-20261022/clone-cost.py /Volumes/CaseSensitive/new-folder
```

`binaries.json` has the commits and SHA-256 hashes of the measured binaries.
