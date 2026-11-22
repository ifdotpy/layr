# Git in fast lines: measurements (2026-11-16)

Machine: kukuka-wsl (WSL2, kernel 6.18, 32 CPUs, Btrfs with zstd, git 2.53). Other work ran on
the machine: load average 7 to 14 during the final run. Tree: the Linux kernel, 97,294 files,
2.0 GB, one commit. The text and the tables are in
[docs/layrfs-lab.md](../../docs/layrfs-lab.md#git-in-fast-lines-2026-11-16).

| File | What |
|---|---|
| `results.jsonl` | the final run: 5 rounds (2 for the classic line) |
| `results-sync-hook.jsonl` | an earlier build: the hook waited for the head, and a save after `git reset` built the tree |
| `results-first.jsonl` | the first build, one round: every save and every move of the head wrote the whole tree |
| `scripts/it.sh` | a root `layr daemon` on a store of its own (`/home/gitview-it`) |
| `scripts/ksetup.sh` | the kernel project: a copy of the tree, a new repository of one commit, `init --from-git`, `migrate` |
| `scripts/bench.sh` | the steps, in a plain git folder, a classic line and a fast line |
| `scripts/trace.sh` | `LAYR_TRACE` of status, `git adopt` and save in a fast line |
| `scripts/table.py` | medians of a results file |

```sh
cargo build --release --features layrfs-fuse
scripts/it.sh up && scripts/ksetup.sh && scripts/bench.sh 5 && scripts/table.py ~/gitview/results.jsonl
```
