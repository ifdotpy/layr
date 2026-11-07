# Virtual lines on Chromium (2026-11-07)

A virtual line (LayrFS mount, docs/views.md) against a folder line (APFS clone) on one project.
Script: [bench.sh](bench.sh). Raw output: [results.txt](results.txt).

## Setup

- MacBook Pro M5 Max, macOS 27.2, internal SSD, APFS (case-insensitive data volume).
- The Mac was shared with other work: load average 30 to 75 during the runs. Absolute times are high; compare the
  two columns of one row.
- Project: a Chromium `src` checkout without `.git`, 515,999 files (`layr init chromium --from <copy>`; the init
  took 32 minutes). The start state is a complete tree.
- Layr built from this branch (release). LayrFS overlay mode with `openclose=off`.
- Each step ran 3 times; the table shows the median in seconds.
- 10 edits: 6 changed files, 2 new files, 1 delete, 1 rename.

## Results

| Step | Folder line | Virtual line |
| --- | ---: | ---: |
| `line new` | 65.8 | 0.29 |
| `status` after 10 edits | 18.1 | 0.34 |
| `save` of the 10 edits | 35.5 | 0.38 |
| `line delete` (after the save) | 81.5 | 0.39 |
| `rg -l TODO` (warm, 31,535 files match) | 21.7 | 254.8 |
| `find . -type f` (warm, 515,999 files) | 7.3 | 17.3 |

- The saved states of both lines are equal (`layr diff --exit-code nat1 virt1`).
- `rg` and `find` give the same results in both lines.
- A virtual line on a thin start state: the first one makes the complete tree of the state (31.6 s, one clone
  of the tree, once per state); the next one takes 0.17 s.
- The folder line's `status` includes its first scan after the edits (the status cache was built before them).

## Reads through the mount

Metadata, line operations and edits are fast. Bulk reads of file content are slow: `rg` over 516k files took
12 times as long through the mount. The extension used less than one CPU core during the run (60 to 80 %),
and a sample shows most time in the read handler, with one temporary `openat` per read once the cached
descriptors (half of the descriptor limit) are in use. The read path needs work before tools that read the whole
tree (search, indexers) use a virtual line; see the open items in docs/views.md.

Two checks after the run, one `rg -l TODO` each on a new virtual line (same load):

| Change | Cold | Warm |
| --- | ---: | ---: |
| Reader-writer lock in the overlay (was one mutex) | 380 | 265 |
| Kernel data cache (`datacache=writeback`, open and close calls on) | 253 | 435 |

Neither changed the result much; the lock change stays, the data cache stays off.
