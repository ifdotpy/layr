# git on v2 (2026-11-19)

Two versions: loose objects (first, `results.jsonl`) and one virtual pack (now, `results-pack-*.jsonl`). The code is the pack version.

Question: can layr keep file contents only in v2 and give them to git on the fly?

Prototype (`src/main.rs`):
- `map`: the git id of each file of every v2 state (SHA-1 of `blob <size>\0` + content).
- `mount`: a FUSE folder that shows each file as a git loose object (`xx/yyyy...`).
  - The object bytes are made at open: zlib "stored" blocks, no compression.
  - Made objects stay in a 1 GB cache.
  - The kernel keeps the folder lists (`FOPEN_CACHE_DIR`).
- Commits and git trees stay a real git pack: 34.6 MB for 1,000 Chromium commits.
- `.git/objects/info/alternates` names the FUSE folder.

`bench.sh` runs each git command two times on the same work folder:
- base: the normal git pack (`chromium.git`, depth 1000);
- v2: the light pack plus FUSE over the packed cold v2 store (1,233 MB).

Machine: kukuka-wsl, 32 CPUs, Btrfs, load 3 to 6. One run.

## Results

Store of 1,000 commits: 518,361 files, 487,377 distinct file contents (4.7 GB raw).

`map` (read and hash every file content of the v2 store, 32 threads): 432 s.
Ids: 487,376 of 487,377 are equal to git's ids. One file differs (see below).

| command | base p1 | base p2 | v2 p1 | v2 p2 | objects made (v2 p1) |
|---|---:|---:|---:|---:|---:|
| `read-tree` + `update-index --refresh` | 29.3 s | 18.0 s | 17.8 s | 19.2 s | 95 |
| `status` | 2.1 s | 1.5 s | 1.6 s | 1.5 s | 0 |
| `log --oneline -n 1000` | 23 ms | 20 ms | 1,932 ms | 159 ms | 0 |
| `log --format=%H -n 1000` | 19 ms | 19 ms | 15 ms | 19 ms | 0 |
| `log -p -n 100` | 747 ms | 713 ms | 10,933 ms | 1,432 ms | 2,476 (98 MB, 8.2 s) |
| `diff HEAD~100 HEAD` | 565 ms | 769 ms | 1,446 ms | 1,220 ms | 0 (cached) |
| `log -p DEPS` (most changed file) | 287 ms | 179 ms | 480 ms | 271 ms | 136 |
| `blame chrome/browser/about_flags.cc` | 336 ms | 275 ms | 294 ms | 311 ms | 3 |
| push pack, 10 commits | 50 ms | 69 ms | 108 ms | 103 ms | 0 (cached) |
| push pack, 100 commits | 321 ms | 310 ms | 962 ms | 759 ms | 52 |
| `archive HEAD` (all files) | 19.5 s | 15.8 s | 252.5 s (failed at the end) | 233.2 s | 442,345 (3.6 GB, 47.8 s) |

p1: first run after the commands above. p2: the same command again.

## What the numbers say

- Commands that read only commits and trees cost the same as git.
- The first command that shortens ids (`--oneline`) lists all 487k objects through FUSE: 1.9 s, one time per mount.
- Making an object from the cold v2 store costs about 3.3 ms (delta chains up to 16, 1,152 packs). Cached objects cost nothing more.
- Reading all files (`archive`, and the same for `clone`, `fsck`, `gc`) is 13 times slower. Making the objects is 48 s of 252 s. The rest is one FUSE open, read and close per object.
- Fix for both (not built yet): show the files as one virtual pack (`.pack` + `.idx`) instead of loose objects. git then needs no folder lists and opens one file.

## The one wrong id

`third_party/mako/mako/test/templates/crlf.html`: the work folder bytes are not the git blob bytes (line end conversion at checkout). v2 keeps the work folder bytes, so the blob git asks for does not exist. The same holds for every clean filter (`eol`, `text`, git LFS). A real build must keep the git blob bytes when they differ from the file.

# Version 2: one virtual pack

Changes against the loose version:
- The FUSE folder shows one pack: `pack/pack-<id>.pack` and `.idx`. The index is made at mount (15.8 MB, 71 ms). An entry is made when git reads its bytes. A unit test checks the index against `git index-pack`.
- The pack holds the files in path order, newest state first. Neighbors in a page are then the files git reads next, and they sit in the same v2 block.
- One shared v2 store, readahead 128 KB (the kernel limit).
- The one blob with other line ends is a real git object in the line.
- Three v2 stores of the same history, with pack blocks of 4 MB (today), 1 MB and 256 KB.

## Store size by block size

| block | packed v2 | + light git pack | read all files (`map`, 32 threads) |
|---|---:|---:|---:|
| 4 MB | 1,233 MB | 1,268 MB | 5.6 s |
| 1 MB | 1,293 MB | 1,328 MB | 7.9 s |
| 256 KB | 1,367 MB | 1,402 MB | 6.1 s |

(`map` in path order. In id order it took 268 s with 4 MB blocks.)

## Commands (ms, first run; one run each, load 4 to 12)

| command | git | v2 4 MB | v2 1 MB | v2 256 KB |
|---|---:|---:|---:|---:|
| `status` | 1,474 to 2,496 | 1,598 | 1,606 | 1,421 |
| `fsck --connectivity-only` | 1,480 to 1,985 | 1,071 | 1,174 | 1,661 |
| `log --oneline -n 1000` | 23 to 31 | 42 | 45 | 46 |
| `log -p -n 100` | 940 to 1,763 | 4,192 | 3,682 | 1,299 |
| `diff HEAD~100 HEAD` | 666 to 1,051 | 740 | 1,043 | 478 |
| `log -p DEPS` | 235 to 691 | 2,450 | 1,913 | 1,963 |
| `blame about_flags.cc` | 358 to 1,152 | 600 | 611 | 316 |
| push pack, 100 commits | 316 to 479 | 509 | 418 | 301 |
| `archive HEAD` (all files) | 29,122 to 39,219 (cold), 16,621 to 20,712 (warm) | 39,872 | 40,321 | 34,907 |

The git column is the range over the three runs. A second run of a command on v2 costs the same as on git (entries stay in the page cache).

## Push size

The pack of 100 commits for a push:

| | git reuses entries | `--no-reuse-object` |
|---|---:|---:|
| git | 9.9 MB, 322 ms | 6.2 MB, 800 ms |
| v2 | 53.1 MB, 914 ms | 6.2 MB, 826 ms |

v2 entries are not compressed, and git sends reused entries as they are. A push from v2 must use `--no-reuse-object`.

## What is left

- `log -p` of one file with many versions: 2 s against 0.2 to 0.7 s. Each version is a v2 delta chain (up to 16 deep).
- Reading all files the first time: about 2 times slower than a warm git pack.
- Memory: kukuka-wsl (19 GB) restarted two times during these runs. The cause is not known.
