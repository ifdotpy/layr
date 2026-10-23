# APFS features, 2026-10-23

Baseline: `2ea2727` (thin APFS states). Candidate: `2a271ef`; the saved status was measured later on the final branch.
See [the feature review](../../docs/apfs-features.md). CPU is user and system time of the command.
The host ran other work, so wall times vary; CPU values are more stable.

## One line, 10,000 files of 4 KiB

`paired.py`, 3 paired trials, medians. The image has no FSEvents log, so this measures bulk scans only.

| Command | CPU before, s | CPU after, s |
| --- | ---: | ---: |
| `save` | 0.31 | 0.21 |
| `add -A` | 0.33 | 0.22 |
| `commit` | 0.19 | 0.12 |
| `status` | 0.32 | 0.19 |
| `diff --stat HEAD~1` | 0.32 | 0.18 |
| `branch` | 0.93 | 0.92 |

Space does not change.

## A 4 KiB change in a 1 GiB file

`large-file.py`: 1,000 files of 4 KiB and one 1 GiB file, one run per binary, warm cache.

| Command | CPU before, s | CPU after, s |
| --- | ---: | ---: |
| `status` | 0.91 | 0.03 |
| `diff --stat` | 0.63 | 0.03 |
| `add -A` | 0.63 | 0.03 |
| `commit` | 0.65 | 0.02 |
| `diff --stat HEAD~1 HEAD` | 0.60 | 0.02 |

With a cold cache the old method also reads 2 GiB from disk.

## Saved status, 100,000 files

`status-cache.sh` on a case-sensitive image, one or two changed files per step.
`full` sets `LAYR_NO_STATUS_CACHE=1`. Results: `status-cache-results.txt`.

| Run | CPU, s | Wall, s |
| --- | ---: | ---: |
| Saved status | 0.22 to 0.24 | 0.22 to 0.25 |
| Full comparison | 1.28 to 1.35 | 1.31 to 1.39 |

The one folder scan takes about 0.22 s of the saved status.

An earlier version answered `status` from FSEvents in 0.01 s CPU (`fsevents.sh`, `fsevents-results.txt`).
It was removed: a write through a memory mapping gives no event, so that status could be out of date.

## Probes

- `probe.c`: bulk attributes, physical extents of a clone, document IDs.
- `fsevents-probe.c`: FSEvents history since a stored ID. On an image attached with `-nobrowse` it returns no events;
  on a browsable image and on the system data volume it returns every change.

`binaries.json` has the commits and SHA-256 hashes of both binaries.
