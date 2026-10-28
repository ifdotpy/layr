# layr-retest: run only the tests that a change can affect

Status: first version built (2026-10-28): the Rust adapter, in `crates/layr-retest`. Design
agreed with the owner on 2026-10-28. Prototype measurements are in the last section. See
"First version" for the command line, the record format and what is not done.

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

## First version

Crate `crates/layr-retest`, binary `layr-retest`. It runs `layr`, `cargo nextest`, `llvm-profdata`
and `llvm-cov` as programs and uses no internal crate of layr. The two LLVM tools come from the
`llvm-tools` rustup component of the active toolchain.

### Command line

Run in the root folder of a layr line that is also the cargo project root.

- `layr-retest run [--full] [-- <nextest args>]`
  1. Saves the working copy (`layr save`, an auto state if it changed). The id of that state, or of
     the head if nothing changed, is the current state.
  2. Baseline: the newest state in `layr log --auto` that has a record file. Without one, or with
     `--full`, all tests run.
  3. `layr diff <baseline> <current> -U0` gives the hunks.
  4. `cargo nextest list` (instrumented build in `target/retest`) gives all tests. Stale tests are
     selected by the Rules. Tests that have no record, and tests that failed, always run.
  5. `cargo nextest run --no-fail-fast -E '<filter of the stale tests>'` runs them. A run of all
     tests has no filter.
  6. For each test that ran: `llvm-profdata merge -sparse`, then `llvm-cov export` (lcov for
     executed lines, JSON for function ranges) on the binary of that test. Four exports run at a
     time. Files outside the project, in `target/`, and in `.cargo`, `.rustup` and `/rustc/` are dropped.
  7. The record is written and the raw profiles are deleted.
  8. The exit code is the exit code of nextest. The last line prints how many tests ran, passed,
     failed and were skipped.
  The nextest arguments after `--` go to both `list` and `run`. Use them to select packages
  (`-p`, `--workspace`). Do not put `-E` there.
- `layr-retest wrap -- <test binary> <args>` is the cargo target runner that `run` installs
  (`CARGO_TARGET_<TRIPLE>_RUNNER`). With `NEXTEST_TEST_NAME` set it sets `LLVM_PROFILE_FILE` to
  `$LAYR_RETEST_PROFILE_DIR/<binary id>/<test>.profraw` (names cleaned to letters, digits and
  `_-.`), then execs the test. In the list phase it sets no profile.

### Record format

File `.layr-retest/<state id>.json` in the line root (the id is the one that `layr log --json`
prints). The folder is in `.gitignore`, so a save does not see it. It is a cache path (see Records in a
cache overlay).

```json
{
  "baseline": "<state id of the record that this run started from, or null for a full run>",
  "tests": {
    "<binary id> <test name>": {
      "status": "pass",
      "deps": ["hex-0.4.3"],
      "files": {
        "src/lib.rs": { "lines": [[1, 3], [7, 7]], "funcs": [[1, 3]], "idle": [[5, 9]] }
      }
    }
  }
}
```

- `lines`: executed line ranges. `funcs`: ranges of functions that the test executed. `idle`:
  ranges of other functions in the file. All ranges are inclusive and use lines of the state
  that the record belongs to.
- A record is complete. A test that did not run keeps its entry from the baseline, with every
  range moved to the lines of the new state through the hunks of the diff, and the file key
  changed after a rename. If a hunk changed the first or last line of a range, the entry cannot
  be moved and the test runs again.
- A full run writes a record with no baseline and no carried entries. Later runs find it first,
  so older records are no longer used.
- `idle` is not in the first design. It is needed to tell a change inside a function that the
  test did not call (no effect) from a change at module level (the test is stale).

### Selection

A test is stale when it has no entry, or failed, or one hunk of a file in its record is one of:

- an old line of the hunk is an executed line, or in an executed function;
- a pure insertion strictly inside an executed function (a new match arm);
- a changed line is outside every function of the file, or a pure insertion outside every function
  is not made only of whole functions, comments and blank lines (a new constant, a type, a field).

A new function that nothing calls, a change inside an idle function and a comment between functions
do not make a test stale. The "whole functions" check reads the added text: a line starts a function
(`fn`, `pub fn`, `async`, `const`, `unsafe`, `extern` before `fn`), then brackets balance. A string
that holds an unbalanced bracket can fool it.

A state with auto states: auto states are siblings (they all have the last named state as parent).
So the baseline search reads the whole line log, not only the ancestors of the current state.

### Dependencies

- Each test record has `deps`: the packages outside the project whose files the test executed.
  A file in `<cargo home>/registry/src/<index>/<name>-<version>/` gives `<name>-<version>`. A file in
  `<cargo home>/git/checkouts/<folder>/<rev>/` gives `git:<folder>/<rev>`. Only files with an
  executed line count.
- If `Cargo.lock` changed, the tool reads it at the baseline and at the current state
  (`layr show <state>:Cargo.lock`). A package entry (name, version, source) of the old file that is
  missing in the new file is removed. A test that used a removed package is stale. A new or
  unchanged package does not make a test stale. A git package matches by the short revision.
- If a `Cargo.toml` or `build.rs` changed in folder D, a test is stale when it executed any file
  under D. For the root folder this means every test that executed any project file.
- A new `Cargo.lock` or `Cargo.toml` (not in the baseline) selects nothing: no earlier test could use it.
- `Cargo.lock` outside the root folder, `.cargo/config.toml`, and files that a test reads are not seen.

### Records in a cache overlay

`.layr-retest` is a cache path of the line, so a new line and `layr merge` take the records
(`docs/cache-overlays.md`). The setup, once per project:

1. In `.layrattributes` of the project: `.layr-retest regenerable`. (This repository has no
   `.layrattributes`; add the file when the project wants it. The project setting
   `layr config regenerable '[".layr-retest"]'` does the same for every line of the machine.)
2. `.layr-retest` must be in `.gitignore` (already true here).
3. Key files: a cache overlay is taken only when its key equals the key of the new line. The key
   has no key files by default, so a lockfile change does not drop the records. If the project
   sets `cache.key-files` (for example to `Cargo.lock`), run once:
   `layr overlay set <overlay> --key-files none`. The overlay name is in `layr overlay list`
   (`cache.<hash>`, folder `.layr-retest`).

After it writes the record, `run` calls `layr save --caches`. That saves the folder as the newest
state of the line's cache overlay (`history=last`, `union-theirs`, readers: trusted machines).
The record also goes under the id of the head when the working copy equals the head, because an
auto state is in no other line. So the flow is: work, `layr save -m`, `layr-retest run`,
`layr line new X <line>`, and `layr-retest run` in X selects only what changed.

The installed `layr` 0.4.0 (2026-10-26) has no cache overlays. Build the one in this repository.

### Contradiction

`run --full` compares each failed test with the entry in the nearest record. If it said `pass`, the
tool prints `CONTRADICTION: <test>` and the state of that record. The full run writes a fresh record, so
the old chain is not used again. The end to end test shows it with a test that reads a data file.

### Tests

- Unit tests (`parse.rs`, `select.rs`): lcov to ranges, `-U0` hunks, function ranges, and each
  selection rule.
- `crates/layr-retest/tests/e2e.rs`: temporary `LAYR_ROOT`, a tiny project with three tests. Runs:
  no record, no change, change of one function, a new uncalled function, a change of a data file,
  full run with a contradiction. About 10 s. Needs `layr`, `cargo-nextest` and `llvm-tools`.
  Run: `cargo nextest run -p layr-retest`.

### Not done

- Only the new line and `layr merge` paths of the cache overlay are tested; peer sync of records
  is not tried.
- A file that a test reads (`include_str!`, fixtures, data files), and a change of the toolchain,
  are found by `--full` only. No end to end test for a lockfile change (it needs a second cached
  version of a package); the rules have unit tests.
- The record key does not include the nextest arguments. A run with other `-p` arguments only
  carries entries of the tests it lists; entries of other packages are dropped.
- Test names are cleaned for profile file names. Two names that differ only in cleaned characters share a file.
- The line root must be the cargo project root. The path of `layr-retest` must have no spaces
  (cargo splits the runner on spaces).
- Cross compilation: the runner variable uses the host triple.
- Python and JS/TS adapters.
- No schedule for `--full`.

## Not decided

- Whether the record folder should be gzip-compressed (5.8 KB per 116 tests in the prototype).
