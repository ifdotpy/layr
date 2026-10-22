# APFS add and commit experiments, 2026-10-21

## Question

How much work can APFS avoid if the index stores only changed files, or a commit uses the prepared index directly?

## Results

All values are medians of three complete trials, in seconds.
For 100,000 files, the complete trials are 0, 2, and 3. The interrupted trial 1 is excluded.

### Real commands

| Files | Changed files | First add | Add with existing index | Commit |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | 1 | 5.938 | 7.838 | 3.543 |
| 10,000 | 100 | 5.962 | 7.349 | 2.941 |
| 10,000 | 10,000 | 15.720 | 21.349 | 5.818 |
| 100,000 | 10 | 85.039 | 108.776 | 76.389 |

### Storage operations only

These numbers are not complete command times. The selected paths are already known.

| Files | Changed files | Clone and validate selected files | Seal and publish delta | Materialize full folder |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | 1 | 0.000806 | 0.000541 | 0.506 |
| 10,000 | 100 | 0.020724 | 0.004409 | 0.514 |
| 10,000 | 10,000 | 1.776392 | 0.405901 | 3.002 |
| 100,000 | 10 | 0.002705 | 0.000799 | 9.434 |

### Reuse a complete index

Compare the paired sum of clone, seal, and old-index removal against seal and rename.
Both choices start with an already prepared complete index.

| Files | Changed files | Clone, seal, remove old index | Seal and rename index | Ratio |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | 1 | 2.173 | 0.567 | 3.83x |
| 10,000 | 100 | 2.523 | 0.783 | 3.22x |
| 10,000 | 10,000 | 1.105 | 0.419 | 2.64x |
| 100,000 | 10 | 16.609 | 4.769 | 3.48x |

### Interpretation

- A repeated add still makes snapshots of both the working tree and the existing index. The trace confirms two complete clones.
- Copying selected files is cheap when few files change. Avoiding full-tree copies is the main candidate for a faster add.
- Reusing a complete index reduces the measured storage phase by 2.6x to 3.8x. This is not a measured speedup of the complete commit command.
- A sparse state avoids most work when few files change, but a complete workspace still requires materialization.
- At 10,000 changed files, selected-file cloning takes 1.776 seconds and materialization takes 3.002 seconds. The sparse approach is less attractive for dense changes.
- Source validation, change discovery, database records, and recovery must still be integrated before claiming a new command time.

### Variation and evidence

The host runs other workloads. One-minute load varies across the recorded samples.
The complete 100,000-file commit samples range from 54.679 to 123.635 seconds.
Do not compare these absolute times directly with earlier APFS or btrfs runs.
The benchmark uses the release binary built from `b0a41a3f7538c66a8e0e8e97f1fc3030b20abfb4`.
The product code is unchanged by this experiment.

- [Raw results](results.json), including the excluded incomplete trial.
- [Medians and ranges](summary.json).
- [Script and binary hashes](source-sha256.json).

## Method

The current release binary runs on a dedicated case-sensitive APFS disk image.
Each case has three trials. The source files contain 4 KiB each.

- 10,000 files: change 1, 100, or all 10,000 files.
- 100,000 files: change 10 files.
- Change each selected file twice. Restore its modification time after each write.
- Measure the first add and a second add with an existing index.
- Use an explicit path for the one-file case. Use `add -A` for the other cases.
- Commit the second version. Check status, run `fsck`, and verify both the new content and the unchanged main line.

The isolated filesystem experiments use the same final content:

1. Clone only the known changed paths. Validate each source with its kernel generation count and metadata before and after the clone. Write a manifest.
2. Make the contents of that small tree immutable, rename the tree, then make its root immutable.
3. Materialize a complete folder from the base and those changes.
4. Clone and seal a complete prepared index, then measure removal of the old index.
5. Seal the contents of a complete prepared index, rename it, then seal its root without a second copy.

Steps 4 and 5 alternate order. Their index preparation is outside the timer.
Content checks and test cleanup are outside the command and prototype timers.
The measurements use a warm or mixed cache. The host runs other workloads.

## Limits

The filesystem experiments are not implementations of `layr add` or `layr commit`.
They omit project authorization, locks, record signatures, database transactions, Git export, overlays, recovery, and conflict handling.
The selected-path experiment receives its file list; it does not discover changes across the project.
It writes a manifest, but it does not define a durable index format or test recovery after power loss.
The prototype does not support deletions, renames, symbolic links, hard links, or arbitrary metadata changes.
A directory rename does not establish that a published state survives a crash.
The prototype's immutable flags do not create valid Layr state metadata.

The full-folder experiment measures a cost that a sparse format moves to later use.
It must not be omitted when a caller needs a complete workspace.

## Reproduce

```sh
python3 benchmarks/apfs-add-commit-20261021/measure.py \
  --binary target/release/layr \
  --root /private/tmp/layr-apfs-add-commit-new
```

Use a new root. The script creates an owned APFS image and removes it after success.
A failed run retains the image for inspection.

## Harness correction

The first run stops after its first real commit because the prototype omits `FSOPT_ATTR_CMN_EXTENDED` from `getattrlist`.
The SDK requires that flag for `ATTR_CMN_GEN_COUNT`. The product implementation already includes it.
`measure-initial.py` and `initial-results.json` retain that attempt. The corrected script passes the generation-attribute preflight.

The second attempt shows that APFS refuses a rename of an immutable root directory.
The final prototype seals the contents, renames the directory, then seals the root.
`measure-v2.py` and `v2-results.json` retain that failed attempt.
The corrected sequence passes all operations and content checks in six trials on a ten-file fixture (`smoke-results.json`).

## Interrupted large run

The 30-minute command limit interrupts trial 1 of the 100,000-file case during commit.
All three 10,000-file cases complete before the interruption.
`interrupted-results.json` retains those measurements and the incomplete large trial.
The resume command keeps completed trial 0 and starts fresh trials 2 and 3 on the same fixture.
Exclude trial 1 from summaries; it has no complete commit or prototype results.
