# Object store measurements of 2026-11-07

Scripts and raw output behind `docs/objects.md`, "Reads, block size and saves". Each `runN.sh` uses release builds of the `objects-bench` and `objects-read` examples, copied to `$TMPDIR/ob-bin`, and a Chromium `src` checkout at `$TMPDIR/layr-chromium/src`.

- `run1`: no named states, so objects in id order. Its read sample ran after `form()` had loaded the block, so its read times are too low.
- `run2`: named states; history with and without deltas; Chromium with blocks of 56 and 4 MiB. Its read sample has the same fault.
- `run3`: cold reads from a newly opened store and saves, by block size. Its saves re-store unchanged files.
- `run4`: the final format, with real changes before each save.
- `run5`: a checkout of Chromium from packs in the final format, compared with the source.
