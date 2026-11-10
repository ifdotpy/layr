# Incremental object store v2 optimization

Base: release-v1-path state a06d674. The change skips native path and metadata access for unchanged sibling entries.

The writer still lists directory names, so additions and removals remain visible. Changed entries retain metadata and error checks.
Tree encodings, object IDs, times, hard-link handling, and the store format do not change.

## Measurement

Apple M5 Max, macOS arm64, APFS, release binaries, four Rayon threads.
Each trial changes five files before each of 30 incremental writes. Each write also publishes a state reference.
Seven trials per binary and layout; before/after order changes between trials.
The table shows the median of seven per-trial medians. The host also ran other work.
These timings measure the object writer, not the entire layr save command. Initial import and full verification are outside the timer.

| Files | Directories | Before, ms | After, ms | Speedup |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 10 | 2.77 | 2.14 | 1.29x |
| 10,000 | 100 | 4.28 | 2.76 | 1.55x |
| 10,000 | 1 | 51.03 | 11.56 | 4.42x |

Every incremental result matches a full write, including tree and times-table IDs.
Paired binaries produce identical final IDs. Raw samples and trial ranges are in results.json and summary.json.

## Checks

- The regression fails before the change: 101 unchanged sibling paths are resolved. After the change: zero.
- A source error for a changed entry still fails the write.
- Object crate: 45 tests passed, including complete and incomplete hard-link groups and directory times.
- t200-objects.sh: 37 passed.
- t202-migrate.sh: 66 passed.
- object-state-parity.sh: passed on macOS/APFS.
- Linux runtime and end-to-end save speed were not measured for this change.

## Reproduce

Build the same harness against the baseline and changed source states in separate Layr lines.
Keep both release binaries, then run:

~~~sh
cargo build --release --offline --manifest-path benchmarks/object-v2-incremental/Cargo.toml
python3 benchmarks/object-v2-incremental/run.py --before /path/to/before --after /path/to/after
~~~

The runner creates isolated temporary stores and removes them after each trial. It does not use the real project store.
