# layrfs-fuse benchmark, 2026-11-15

Raw data of [docs/layrfs-fuse.md](../../docs/layrfs-fuse.md) for `layr fs view` with `layr tool`
answers from the view's index, the git fsmonitor hook and the kernel caches. Machine: kukuka-wsl
(WSL2, Linux 6.18, Ryzen 9 5950X), the same bench setup as
[layrfs-fuse-20261114](../layrfs-fuse-20261114/README.md).

- `layr-<round>.jsonl`, `linux-<round>.jsonl`: one line per step, rounds 1 to 3, written by
  `tools/layrfs-fuse/bench/bench.sh` (through `run3.sh`). The docs give the median of the rounds.
- `load.txt`: the load average and the busiest processes before each tree of each round. Round 2
  was started twice: the first run stopped at a busy OverlayFS umount (the user's index server
  walked the line), and its partial results are not used.
- `check.log`: `tools/layrfs-fuse/bench/check.sh` on the source of the measured build (43 checks).
- `SYSTEM_CHANGES`: every change to the test machine (this run added the install of
  `/home/layr/layr`).
