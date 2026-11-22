# Store v3 prototype against store v2 (2026-11-18)

Question: can the source states of layr be git objects (one store for layr and git) without a
loss against object store v2?

## v3 in this prototype

- A state is a git commit. Its tree is the source tree (git blobs and trees).
- Git keeps only the modes 644, 755 and link. The exact mode and the time of each entry are in a
  meta table: one blob `.layr-times` per folder (own time, then time and mode per entry in git
  order), in a "times tree" that mirrors the folders. Refs keep the commit and the times tree.
- Objects are written with gix (loose, zlib). `git repack -a -d` packs them.
- v2 is `layr-objects` as layr uses it: `hot` (blobs are clones of the files) and `cold` (zstd).

`src/main.rs` (throwaway), `run.sh` (trials, engine order turns), `table.py` (medians).

## Method

kukuka-wsl (WSL2, 32 CPUs, Btrfs). Corpus: the Linux kernel, 97,294 files, 2.0 GB, without
`.git`. Each run: a reflink copy of the corpus; the first write; 10 saves, each after an edit of
10 files spread over the tree; a diff of the first and the last state; a sequential read of every
blob (second pass); a checkout of the last state; pack; read and checkout again from packs. Both
checkouts are compared with the work folder (content, modes, links, times). 3 trials per engine.
Load of the host 6 to 27 during the runs (other work): treat differences under 30% as noise.

## Results (medians, `table.md`)

trials: v2-hot 3, v2-cold 3, v3 3
| step | v2-hot | v2-cold | v3 | v2-hot + git (today) |
|---|---:|---:|---:|---:|
| first write (ms) | 13,853 | 11,072 | 11,619 | 25,472 |
| save of 10 files (ms, median of 10) | 12 | 12 | 14 | 26 |
| diff first to last state (ms) | 16 | 14 | 17 |  |
| read all files, loose (ms) | 2,098 | 3,500 | 3,144 |  |
| checkout, loose (ms) | 9,425 | 6,822 | 4,092 |  |
| pack (ms) | 62,230 | 48,927 | 50,402 | 112,632 |
| read all files, packed (ms) | 2,505 | 2,499 | 2,507 |  |
| checkout, packed (ms) | 6,005 | 5,831 | 3,750 |  |
| store before pack, allocated (MB) | 1,960 | 656 | 794 | 2,753 |
| store after pack, allocated (MB) | 192 | 192 | 306 | 498 |

"v2-hot + git (today)": layr writes v2 objects and also the git store (export of each state);
the sum of the v2-hot and v3 columns approximates that cost.

## What the numbers say

- Write, save, diff and read cost the same in both formats (within noise).
- Checkout of v3 is faster here, but it is the parallel writer of this prototype, not the format.
- Packed, v3 takes 306 MB against 192 MB for v2 (zlib against zstd, git's delta window).
- Today layr keeps both: 498 MB packed and two writes of each state.

## Not measured

History depth (one commit plus 10 saves), cache overlays (they stay in v2 chunks), LayrFS
reads, cloud transfer, merges, and git with larger windows (`repack -f --window=250`).

## History: Chromium, 1,000 commits (2026-11-18)

`store-v3-prototype history <engine> chromium.git <work> <scratch> 1000` (`results-chromium.jsonl`).
A shallow clone (`--depth=1000`) of chromium/src: 518,361 files in the first commit, 11,463
changed paths over 999 commits (about 20 hours of history). Git writes each commit into the work
folder (`checkout`, not timed); the engine saves the changed paths (timed). v2 is cold (zstd and
deltas against the parent), the end state of a v2 store after repack. One run per engine; host
load 8 to 60 (other work), so times are rough.

| Step | v2-cold | v3 |
|---|---:|---:|
| first write | 127 s | 118 s |
| 999 saves, total | 26.7 s | 17.8 s |
| save, median / p95 | 19 / 63 ms | 10 / 43 ms |
| store before pack | 3,642 MB | 4,186 MB |
| pack | 400 s | 444 s |
| **store after pack** | **1,233 MB** | **1,548 MB (+26%)** |
| checkout of the last state | 172 s | 90 s |
| checkout equals the work folder | yes | yes |

The pack that the Chromium server sent for the same 1,000 commits: 1,464 MB. The v3 tree of the
last state is not git's tree: git keeps 273 submodules as gitlinks, the prototype as empty folders.
