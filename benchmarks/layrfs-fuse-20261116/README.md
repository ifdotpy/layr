# layrfs-fuse benchmark, 2026-11-16

Raw data of the "Version 2" section of [docs/layrfs-fuse.md](../../docs/layrfs-fuse.md). Machine:
kukuka-wsl (WSL2, Linux 6.18, Ryzen 9 5950X, 32 threads), the setup of
[layrfs-fuse-20261114](../layrfs-fuse-20261114/README.md). The view ran with
`--no-open --uring`.

- `<tree>-<round>.jsonl`: one line per step, rounds 1 to 3, written by
  `tools/layrfs-fuse/bench/bench.sh` with `BACKENDS="fuse btrfs"`.
- `load.txt`: the load average after each round (15 to 22: other users' jobs ran).
- `SYSTEM_CHANGES`: the changes of this run to the test machine.
