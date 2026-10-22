# APFS features, 2026-10-23

Baseline: `2ea2727` (thin APFS states). Candidate: `2a271ef`.
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

## FSEvents status, 100,000 files

`fsevents.sh` on a browsable case-sensitive image (it has an FSEvents log), one changed file per step.
`full` sets `LAYR_NO_WATCH=1`.

| Run | CPU, s | Wall, s |
| --- | ---: | ---: |
| FSEvents | 0.00 to 0.01 | 0.10 to 0.24 |
| Full read | 3.12 to 3.46 | 8.97 to 10.79 |

Most of the FSEvents wall time is the wait for the cookie event (20 to 480 ms in these runs).

## Probes

- `probe.c`: bulk attributes, physical extents of a clone, document IDs.
- `fsevents-probe.c`: FSEvents history since a stored ID. On an image attached with `-nobrowse` it returns no events;
  on a browsable image and on the system data volume it returns every change.

`binaries.json` has the commits and SHA-256 hashes of both binaries.
