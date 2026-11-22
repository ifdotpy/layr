# layrfs-fuse benchmark, 2026-11-14

Raw data of [docs/layrfs-fuse.md](../../docs/layrfs-fuse.md), from kukuka-wsl (WSL2, Linux 6.18, Ryzen 9 5950X).

- `layr.jsonl`, `linux.jsonl`: one line per step, written by `tools/layrfs-fuse/bench/bench.sh`. Steps with the suffix `r2` are the second build round.
- `gitstatus-warm.txt`: the warm `git status` runs that `bench.sh` failed to record for the FUSE view of the kernel.
- `check.log`, `check--no-open.log`: `tools/layrfs-fuse/bench/check.sh` in both modes.
- `load-*.txt`: the load average and the busiest processes at the start of each run.
- `SYSTEM_CHANGES`: every change to the test machine.

The default FUSE runs used the build before `--no-open` existed; the `--no-open` runs and the
second build round used the build with it. The final source (formatted) passes `check.sh` in both
modes.
