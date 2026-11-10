# Virtual Git pull and release-build measurements

Source before: `cbd9b0e`. Source after: `68ebb344cef47425b02d01a1bce41586`.
The comparison includes the Git command whitelist, pull support, v2 Git state mapping,
and conflict checks during a named virtual save. The `.git` layout stays unchanged.
The later `01f4fd8` fix enforces `--expect` during pull replay. It changes none of the measured commands.

## Method

Both binaries use Rust 1.99.1 and the Cargo release profile, with four build jobs.
The baseline has its own Layr line, `git-perf-baseline`.
The after build reuses its dependency output, after the baseline binary is copied away.
`release-source.patch` records the production source difference.

`release-paired.py` creates a private v2 store and one FSKit mount for each file count.
It exports the initial state with the after binary before measurements.
Each sample changes five files of about 600 bytes, then runs status, diff, and save.
The complete process time includes process start and command output capture.
Each save must leave a clean status. The mount must contain no `.git` file or directory.

The test runs two warmup pairs, then 15 measured pairs. It alternates binary order.
Both binaries use the same mount and growing history. This controls mount differences;
it does not eliminate machine load or history effects. The JSON records each sample,
load averages, binary hashes, and the private store path.

Reproduce on a native macOS user with the LayrFS extension active:

```sh
python3 benchmarks/virtual-git/release-paired.py \
  --before /path/to/before/layr --after /path/to/after/layr \
  --rounds 15 --files 1000 10000 --out /tmp/release-paired.json
```

The script hides each mount on completion or failure. It retains the private store.
This experiment measures ordinary local operations after Git export.
It does not measure network latency, Linux speed, or pull speed.

## Run conditions

The initial 1,000-file case overlaps another Mac build. Keep it as shared-load evidence only.
The 10,000-file timed samples start after that build.
The separate `release-paired-1000-quiet.json` repeats 1,000 files after local builds and tests finish.
The repeated run also records wall-clock sample times. Both runs retain per-sample load averages.

## Functional validation

`pull-validation/` contains logs from the source after revision on native macOS:

- `views-git.sh`: export, push, fetch, hidden lines, and folder conversion.
- `views-git-pull.sh`: fast-forward, merge, both replay flags, local edit protection,
  review expectations, conflicts, v2 push/fetch roundtrip, and hidden mount cleanup.
- `t03-git.sh`: 70 passed.
- `t08-gitcompat.sh`: 28 passed.
- `t02-merge.sh`: 50 passed.
- `t62-merge-git.sh`: 20 passed.
- `t135-replay-after-merge.sh`: 12 passed.
- `layr-pull-expect-contract.log`: the full FSKit pull contract passes again on `01f4fd8`,
  including refusal of the wrong replay expectation and success with the correct state.

The shell suites use a case-sensitive APFS test volume. The FSKit contracts use
`/private/tmp` with a private `LAYR_ROOT`. All run as the native non-root user.

## Results

Medians of 15 samples per binary, in milliseconds. The final column is the median
of the 15 paired percentage changes; it differs from the ratio of separate medians.

| Files | Command | Before, ms | After, ms | Change of medians | Paired change |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1,000 | `status` | 608.9 | 593.7 | -2.5% | +0.7% |
| 1,000 | `diff` | 608.3 | 599.7 | -1.4% | +1.5% |
| 1,000 | `save` | 673.3 | 680.1 | +1.0% | +0.2% |
| 10,000 | `status` | 6309.6 | 6242.2 | -1.1% | -0.4% |
| 10,000 | `diff` | 6293.1 | 6386.6 | +1.5% | +2.3% |
| 10,000 | `save` | 6421.2 | 6582.7 | +2.5% | +1.8% |

In these source-file workloads, the median save increase is 1.0% for 1,000 files
and 2.5% for 10,000 files. Status and diff medians change by less than 3%.
These measurements show no substantial median slowdown in the tested cases.
They do not establish a bound for other file sizes, change counts, or machine loads.
The absolute 10,000-file times remain several seconds in both builds.

The latest source also includes `1f5db2a`, which fixes missing conflicts in Linux virtual status.
The Linux integration run found that defect with the existing pull contract.
The final fix passes on Linux x86_64 and arm64, on both Btrfs and OverlayFS.
Each architecture also passes 421 CLI tests and 45 object tests (16 CLI tests ignored).
The tested integration state is `de557ab911ff756c891f01a1bcfc2a48`.
`pull-validation/linux-results.json` records result lines and SHA-256 hashes of the complete reports.
