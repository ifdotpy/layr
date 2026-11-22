# Git index in fast lines, and `.git` inside LayrFS (2026-11-17)

Host: kukuka-wsl (WSL2, kernel 6.18, Btrfs, 32 CPUs, shared: load 13 to 43 during the runs).
Tree: the Linux kernel, 97,294 files, a 10 MB git index. Medians in ms.

## 1. Old against new (`ab.sh`, 7 rounds, the two builds alternate in each round)

- old: state `0d21b45` (whole index; after a layr move, layr copies its kept index into `.git`).
- new: `core.splitIndex` in `.git` of a fast line; after a layr move, `git reset --no-refresh`
  as the line owner makes the index of HEAD from git's own index.

| Step | old | new |
|---|---:|---:|
| `git status` (warm) | 65 | 61 |
| `git status` after 10 edits | 114 | 77 |
| `git diff --stat` | 51 | 53 |
| `git commit -a` (10 files) | 152 | 112 |
| `git status` after `git commit` | 122 | 74 |
| `git reset --hard HEAD~1` | 237 | 178 |
| `git status` after `git reset` | 135 | 90 |
| `layr save -a` (10 files) | 1,988 | 1,828 |
| `git status` after `layr save` | 248 | 132 |
| head follows `git commit` (hook in the background) | 2,882 | 2,829 |

The `layr undo` row in `ab.jsonl` failed in both builds (the script passed `-q`, which `layr undo`
does not take; the saved script is fixed).

## 2. `.git` inside the mount (`gitin.sh`, `gitin3.sh`)

The same fast line; git with its folder outside (native, as layr makes it) and with a copy of
the folder inside the LayrFS mount. Hooks off.

| Step | outside | inside | inside, split index | inside, split index, `objects` native |
|---|---:|---:|---:|---:|
| `git status` (warm 2) | 88 | 143 | 92 | 95 |
| `git status` after 10 edits | 145 | 144 | 96 | 101 |
| `git commit` (10 files) | 143 | 215 | 160 | 110 |
| `git reset --hard HEAD~1` | 246 | 320 | 240 | 218 |

(`gitin3.sh`, 7 rounds, load 25.) The cost inside comes from two writes through FUSE (git
trace2 and the counters of the view, `gitdiag2.sh`):

- the whole index (10 MB) at each `git status` and `git commit`: a split index removes it;
- about 30 loose tree objects per commit (`mkdir`, `create`, `rename` each): an `objects`
  folder outside the mount removes it.

`core.fsync=none` changed nothing. `index.skipHash` together with `core.splitIndex` makes git
2.53 abort (`BUG: fsmonitor.c:27`); layr does not set `index.skipHash`.

## 3. `git worktree add` (`wt1.sh` to `wt3.sh`)

| | plain git | hook makes a fast line at the path |
|---|---:|---:|
| `git worktree add` | 12,780 to 13,737 | 1,399 |
| `git status` (warm) in the new folder | 272 to 402 | 43 |

`wt2.sh`: when a hook fills the new folder and its index while `git worktree add` runs, git
writes no file (same inodes). Without the index, git writes all files again.
