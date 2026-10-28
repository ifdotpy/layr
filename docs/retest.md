# layr-retest: run only the tests that a change can affect

Status: design, agreed with the owner on 2026-10-28. Prototype measurements are in the last
section. Not built yet.

## Goal

A test that passed must not run again while the code it executed stays the same, in any line.
The goal is speed only. A saved pass is not evidence for a merge, a release or merge protection;
those keep using `check` attestations of layr (`docs/checks.md`).

A wrong saved pass skips a test that would fail, so it hides a defect. A periodic full run is the
safety net: `layr-retest --full` runs every test and checks the saved passes (see Contradiction).
The user selects the schedule of the full run.

## Rules

- The key is code only. The environment (tool versions, environment variables) does not reset a
  pass. A full run finds a change of behavior that the environment caused.
- A lockfile change resets only the tests that executed code of a changed package.
- A test must give the same result on the same code. A test that does not is a defect of the test;
  the tool does not hide it with repeated runs.
- The smallest unit is the best unit: a line of code where the tool can see it, else a function,
  else a file.
- A line added outside every function that a test executed has no effect on that test. A changed
  or added line inside a function that the test executed makes the test run again. A change at
  module level (constants, types, struct fields, trait implementations) makes every test run again
  that executed any line of that file.
- A test without a saved pass, and a test that failed in its last run, always runs.

## Flow of one run

1. If the working copy has changes, `layr save` makes an auto state (about 14 ms, see
   `docs/cloud-rfc.md`).
2. Find the baseline: the nearest ancestor state with a saved pass of this check.
3. `layr diff <baseline> -U0` gives the changed hunks.
4. The adapter of the test tool selects the stale tests (see Adapters).
5. The test tool runs only the stale tests, with a filter that it already supports.
6. The tool saves the pass and the new coverage of the tests that ran (see Records).

## Adapters

The tool reuses existing test tools. Its own code is the adapters and the record format.

| Language | Tool                                            | Unit            |
| -------- | ----------------------------------------------- | --------------- |
| Rust     | own adapter on cargo-nextest and LLVM coverage   | line, function  |
| JS/TS    | `vitest related <files>`, `jest --findRelatedTests` | file (import graph) |
| Python   | `pytest --testmon` (own database in an overlay) | block, function |

Vitest and Jest select tests from the import graph. They do not see files that a test reads from
disk. The full run catches such a change.

### Rust adapter

No maintained tool exists. `cargo-difftests` (0.6.1, last change 2024-05) has the right mechanism
but needs nightly Rust, a call in every test, and git. The adapter uses only stable parts:

1. Build the tests with `RUSTFLAGS="-C instrument-coverage"` in a separate target folder.
2. Set the cargo target `runner` to a small wrapper. nextest starts every test in its own process
   through this runner and gives it `NEXTEST_BINARY_ID` and `NEXTEST_TEST_NAME`. The wrapper sets
   `LLVM_PROFILE_FILE=<dir>/<binary>/<test>.profraw` and starts the test. In the list phase the
   test name is not set; the wrapper then writes no profile.
3. After the run: `llvm-profdata merge` and `llvm-cov export -format=lcov -skip-functions` on the
   binary of that test give its executed lines; the JSON export gives the line ranges of functions.
4. Select: a test is stale when a changed hunk intersects its executed lines, or lies inside a
   function that it executed, or is at module level in a file that it executed, or is a lockfile
   change of a package whose files it executed.
5. Run the stale tests with a nextest filter: `cargo nextest run -E 'test(=a) | test(=b)'`.

The `llvm-tools` component of rustup gives `llvm-profdata` and `llvm-cov`.

## Records

- One file per check and state in an overlay, for example `retest/<check>/<state>.json`: the
  baseline, the tests that ran, and for each test its executed files with line ranges and the
  function ranges. Names are unique, so a merge of two lines only adds files and has no conflicts.
- A pass of selected tests is a baseline for the next run. A full run starts a new chain.
- Databases of other tools (for example `.testmondata`, location from `TESTMON_DATAFILE`) are
  files in the same overlay. They are not merged: a line takes the file of its parent. If the file
  is out of date, the tool runs the tests again, so this is safe.
- Readers of the overlay are the trusted machines, as for caches. Records move between machines
  with the normal peer sync.

## Contradiction

If a full run fails a test that a saved pass in the chain covers, the tool deletes the records of
that chain and reports the test. The cause is a missing input in the key or a test that does not
give the same result each time.

## Place

A separate crate `layr-retest` in this repository. It uses only the public CLI of layr. The core
of layr has no terms for tests, coverage or adapters.

## Prototype measurements (2026-10-28, this Mac, Rust 1.99.1, nextest 0.9.146)

- The cargo target `runner` receives `NEXTEST_TEST_NAME` and `NEXTEST_BINARY_ID`; each test wrote
  its own profile. The nextest wrapper-scripts feature is not needed (it is experimental).
- `layr-model` and `layr-store`: 124 tests, 2.0 s run, 43 s cold instrumented build.
- Raw profiles: 1.5 MB per test (temporary). Reduced records (executed line ranges in project
  files): 34 KB for 116 tests, 5.8 KB after gzip.
- Coverage export: 0.5 s of CPU per test with the binary of that test only (1.8 s with all
  binaries). This is the main cost; it runs only for the tests that ran and is parallel.

## Not decided

- The record format in detail and the tool's command line.
- Function ranges from the JSON export: check that they cover closures and generic copies.
