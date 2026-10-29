# layr-retest: run only the tests that a change can affect

Status: first version built (2026-10-28): the Rust adapter, and the vitest, jest and pytest adapters (not tried with the real tools), in `crates/layr-retest`. Design
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

## No build when nothing can change

A run that selects no test must not build. After the baseline and the hunks are known, the tool
reads the test list from the baseline record (it lists every test of its state) and skips `cargo`
and `nextest list` when all of these hold:

- every recorded test is carried (none is stale, none failed, none is missing a range end);
- the baseline record has the same nextest arguments as this run (`args` in the record);
- `layr diff --name-status` shows only files that cargo does not read, or Rust files with a change
  of blank lines and `//` comments only. A new, deleted or renamed `.rs` file, any other change of a
  `.rs` file, `Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/` files, `rust-toolchain*` and
  `nextest.toml` need the build and the test list.

Why these limits: a new test can only come from a new or changed Rust file (a `#[test]`, a macro
call, a new target in `Cargo.toml` or `build.rs`), so those changes list the tests again. A code
change in a function that no test ran also builds, because the build is the only check that the
code compiles. A skipped build does not compile the code: it only runs when the code has no
change of meaning. A `//` line inside a multi-line string literal is read as a comment (the
same limit as the insertion rule in Selection).

The run then writes the moved record, prints `N tests, 0 selected, N skipped (incremental, no
build)` and exits. A run with no change at all does not even write a record.

Building only the crates that have selected tests is not done. A change in crate A can break a
dependent crate B that has no selected test, and only a full build finds it. The build with
selected tests stays one build of the whole selection of the nextest arguments.

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
   binary of that test give its executed lines. The line ranges of functions come from one JSON
   export per binary and the names of the executed functions from `llvm-profdata show` (see
   "Coverage export cost").
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

## Measurements on the layr workspace (2026-10-28, this Mac, state a5310ad)

`layr-retest run -- --workspace --exclude layr-retest`, 261 tests, 4 test threads, 4 build jobs.
The tests of `layr-retest` itself are excluded: its end-to-end test calls `layr` from `PATH`.

| Run | Selected | Time |
| --- | --- | --- |
| First run, cold instrumented build | 261 | 330.6 s (the tests themselves: 13.5 s) |
| No change | 0 | 15.5 s |
| One function body changed in `layr-model` | 2 | 46.0 s |
| `--full` after a change (warm build) | 261 | 410.0 s |

After the faster export (2026-10-28, same Mac, 262 tests, load 12 to 40 at the start, so the numbers
move with the load of the machine; the gate lock was taken):

| Run | Selected | Before | After |
| --- | --- | --- | --- |
| `--full`, warm build | 262 | 199.7 s (load 29) | 98.4 s to 105.0 s (load 12 to 33) |
| No change | 0 | 15.5 s (first version, quiet machine) | 46.2 s (load 45, not comparable) |
| One function changed (the probe below) | 2 | 46.0 s (first version) | 58.6 s (load 45) |

The `--full` rows are the clean comparison: the same machine within one hour, the old and the new
binary. The 410 s of the first version was taken on a busier machine. The export of 262 tests now
takes about 85 s of the 98 s. The remaining cost is the lcov export of each test (0.4 s of CPU) and
the build of the tests.

Selection check: a `panic!` at the top of `layr_model::diff::is_binary`. The incremental run
selected 2 tests and both failed; the `--full` run failed the same 2 tests and passed the other
259. Repeated with the faster export: the same 2 tests failed in both runs. No stray `.profraw` file was left in the line.

The coverage export dominated a run with many tests: the `--full` run took 410 s, the tests 13.5 s of it.

### Coverage export cost

Measured on one test of `layr-model` (binary 25 MB, 2480 files with the registry crates, load
about 30; the numbers move by 20 % with the load of the machine):

| Step | Time |
| --- | --- |
| `llvm-profdata merge` | 0.05 s |
| `llvm-cov export` lcov, `-skip-functions` | 0.4 to 0.55 s |
| `llvm-cov export` JSON (text), all functions | 0.4 to 0.65 s, 58 MB of output |
| `llvm-profdata show --covered --all-functions` | 0.01 to 0.03 s, 15 KB |
| `-num-threads` 1, 4 | no gain or slower |

So a test cost about 1.2 s of CPU, and the JSON is 58 MB that was parsed again for every test. The
bin targets are one more 100 MB object when `-object` is used; with it a test cost about 1.8 s.

Method now:

- The function ranges and the idle ranges do not depend on the test. They are read once per test
  binary from the JSON of the first test of that binary (`-skip-expansions`, only files of the
  project kept), and stored as a table: function name, file, line span.
- Per test: `llvm-profdata show --covered --all-functions` lists the symbols of the functions with a
  count above 0. A span is executed when any function with that span ran, as before. Lines still come from
  the lcov export of that test, so the line granularity is the same.
- The `-object` bin targets are exported only when the profile folder of the test has more than one
  profile file, that is when a child process ran.
- Tried and dropped: one JSON export per test for both lines and functions (the JSON is the
  slow and large part, and the lcov is smaller to parse), more workers (8 workers on 18 CPUs showed no clear gain under a varying load),
  and `-num-threads`.
- The record content is equal: on the layr workspace the records of the old and the new method are
  equal for all 262 tests.

## Measurements 2026-10-29: Studio crate, where the time goes

Crate `studio-runtime-process` (146 tests, nextest profile `ci`, warm `target` and `target/retest`)
in Studio line `retest-gate` (`target` is 19.6 GiB, the record folder 11 MiB), this Mac, builds with
`CARGO_BUILD_JOBS=4`. Load: 18 to 105 (1-minute average, other agents were running; each row
says its load). Times are seconds. "Before" is the binary of layr d730ca3 with step timings added.

| Step | Before, no change (3 runs, load 19 to 49) | Before, one line (pre-saved, load 42) | After, one line (pre-saved, load 23) |
| --- | --- | --- | --- |
| save, status, baseline, diff | 4.7 to 11.5 | 7.6 | 6.8 |
| build and list | 1.3 to 2.1 | 5.8 | 12.1 |
| run selected tests | 0.0 | 1.3 | 1.8 |
| export coverage | 0.2 | 3.8 | 6.3 |
| record | 0.5 to 1.0 | 0.8 | 2.0 |
| `layr save --caches` | 52 to 73 | 161 | 0 (not called without `--save`) |
| total | 66 to 83 | 180 | 29.1 |
| plain `cargo nextest run`, same change | 19 to 24 | 29 | 26 |

What the numbers say:

- The first version did not lose its time in two builds. A warm no-change run spent 1.3 to 2.1 s in
  `cargo`, and 52 to 73 s in `layr save --caches`, which saves every cache of the line (the 19.6 GiB
  `target` too). That was 80 % of the run.
- `layr save --caches --cache .layr-retest` saves the record folder only, but it still makes a state of
  the line: 75 s in this line (load 28). So the save is off by default and `--save` runs it
  once at the end of a series. It holds the lock of the store while it runs.
- `layr save` of a changed working copy takes 38 to 57 s in this line (3.7 s in the layr line, whose
  `target` is 3.4 GB), and 100 s or more when another session holds the store lock. This is layr, not
  layr-retest, and it is the same cost that a `layr save -m` of the agent pays. The table
  pre-saves the change, so that the save step is only the status check. `layr status --json` took
  4 s (5.6 MB, it lists 49 000 ignored files); the tool now reads `layr status --porcelain` (0.25 s).
- One changed line: 29 s against 26 s for the plain run in the same minute. The selected tests take
  1.8 s instead of 15 s, but the instrumented build of the changed crate costs 12 s (the plain build
  about 5 s), the coverage export 6 s and the record 2 s. The floor of this design is: instrumented
  incremental build + export of every selected test (0.4 s of CPU each, 4 at a time). It wins when
  many tests are skipped or the tests are slow; it does not win on a crate whose tests take 15 s.
- The gate's plain nextest step is already replaced by the retest step when `STUDIO_RETEST=1`, so
  there is no second test build in the gate for this step.

Layr workspace (262 tests, `--workspace --exclude layr-retest`, load 38 to 56): full run with the
build 272 s (build and list 126 s, export 102 s); no change after it 30 s (a `Cargo.lock` change
sent it through the build); the probe check below.

Selection check with `panic!("probe")` at the top of `layr_model::diff::is_binary`: the incremental
run selected 2 tests and both failed; `--full` failed the same 2 (`interdiff::tests::patch_format_keeps_modes_binary_renames_and_missing_newlines`,
`interdiff::tests::same_patch_has_no_interdiff_and_amendment_has_file_counts`) and passed 260.

## First version

Crate `crates/layr-retest`, binary `layr-retest`. It runs `layr`, `cargo nextest`, `llvm-profdata`
and `llvm-cov` as programs and uses no internal crate of layr. The two LLVM tools come from the
`llvm-tools` rustup component of the active toolchain.

### Command line

Run in the root folder of a layr line that is also the cargo project root.

- `layr-retest run [--full] [--save] [-- <nextest args>]`
  `--save` ends the run with `layr save --caches --cache .layr-retest` (after step 7), which puts the
  records in the cache overlay so that a new line takes them. It is off by default: the call makes a
  state of the line and holds the lock of the store (75 s in the Studio line, see Measurements 2026-10-29).
  Run it once at the end of a series of runs, or when a new line is about to start.
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
     executed lines) on the binary of that test, and `llvm-profdata show --covered --all-functions`
     for the executed function names. Function ranges come from a table that is built once per
     test binary. Four exports run at a time. Files outside the project, in `target/`, and in `.cargo`, `.rustup` and `/rustc/` are dropped.
  7. The record is written and the raw profiles are deleted.
  8. The exit code is the exit code of nextest. The last line prints how many tests ran, passed,
     failed and were skipped.
  Before step 4 the fast path of "No build when nothing can change" can end the run.
  Every step prints `layr-retest: step <name>: <seconds>s`.
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
  "args": ["--workspace"],
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

With `--save`, `run` calls `layr save --caches --cache .layr-retest` after it writes the record. The `--cache`
option saves only that cache path: `layr save --caches` alone also saves every other cache of the line,
and the build folder of a big project (19.6 GiB for Studio) takes 50 to 160 s. A layr without
`--cache` makes this call fail; the run then prints a warning and keeps the record files. The call
saves the folder as the newest
state of the line's cache overlay (`history=last`, `union-theirs`, readers: trusted machines).
The record also goes under the id of the head when the working copy equals the head, because an
auto state is in no other line. So the flow is: work, `layr save -m`, `layr-retest run --save`,
`layr line new X <line>`, and `layr-retest run` in X selects only what changed.

The installed `layr` 0.4.0 (2026-10-26) has no cache overlays. Build the one in this repository.

### Processes that a test starts

- The wrapper gives each test a folder `<profile dir>/<binary id>/<test>/` and sets
  `LLVM_PROFILE_FILE` to `<folder>/%p.profraw`. Each process of the test (the test itself and any
  child that keeps the environment) writes its own file. All files of the folder are merged.
- When the folder has more than one profile file, the export uses the test binary and, with
  `-object`, every bin target of the workspace (`cargo metadata`, built in
  `target/retest/debug/<name>`). So code that runs only in a child process is in the record of the
  test that started the child. With one profile file only the test binary is exported: the one
  process was the test itself.
- `cargo nextest list` and `run` have `LLVM_PROFILE_FILE=<profile dir>/other/%p.profraw`, so the list
  phase and build scripts write no file in the project.
- Limit: a child started with a cleared environment writes `default_*.profraw` in its working
  folder. After the run the tool deletes every `.profraw` file outside `target/` that is newer than
  the start of the run, and prints how many. That coverage is lost: code that only that child ran
  is not in the record, so a change of it does not select the test. `--full` finds such a change.

### JS/TS and Python adapters

`layr-retest run --tool vitest|jest|pytest [--full] [-- <tool args>]`. The default tool is
`nextest`. These tools choose the tests themselves. layr-retest saves the working copy, finds the
baseline, gives the tool the changed files, and keeps the record.

- Baseline: the newest state with a record of this tool (`.layr-retest/<tool>/<state>.json`:
  `tool`, `baseline`, `status`). The nextest records stay in `.layr-retest/<state>.json`.
- vitest: without a baseline, or with `--full`: `<cmd> run <args>`. Else
  `<cmd> related --run <changed files> <args>`.
- jest: without a baseline, or with `--full`: `<cmd> <args>`. Else
  `<cmd> <args> --findRelatedTests <changed files>`.
- Changed files: `layr diff <baseline> <current> --name-only`, only files that still exist, and
  not the record folder. If no file changed, nothing runs and the run passes (a record is written).
- pytest: `<cmd> --testmon <args>` with `TESTMON_DATAFILE=.layr-retest/testmondata`, so the data
  file is in the cache folder and travels with the records. `--full` uses `--testmon-noselect`.
  No file list is needed.
- A record is written only when the tool exits with 0. After a failed run the next run starts
  again from the last passing baseline, so the files of the failed run are given again.
- The command: `LAYR_RETEST_VITEST`, `LAYR_RETEST_JEST`, `LAYR_RETEST_PYTEST` replace the default
  (`npx vitest`, `npx jest`, `pytest`), for example `LAYR_RETEST_VITEST="pnpm exec vitest"`.
- Limits: vitest and jest use the import graph, so a file that a test reads from disk is not seen
  (`--full` finds it). The records of these tools hold no coverage. Not tested with the real tools:
  vitest, jest and pytest-testmon are not installed on this machine (`npx` and `node` are), and
  nothing was downloaded. The end to end test `vitest_adapter_with_a_fake_tool` uses a script that
  logs its arguments and exits with a code from a file: first run, no change, one changed file,
  a failing run, `--full`. Argument building of all three tools has unit tests (`tools.rs`).
  The jest and pytest run paths share the code of the vitest path and are checked by unit tests only.

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
- No schedule for `--full`.
- Child processes only have coverage when the child is a bin target built in `target/retest`. A test
  that starts a binary from another folder (Studio's gate sets `STUDIO_SUPERVISOR_BINARY` to the
  plain `target/debug` build) runs code that has no instrumentation, so a change of that binary does
  not select the test.

## Not decided

- Whether the record folder should be gzip-compressed (5.8 KB per 116 tests in the prototype).
- A faster coverage export: the lcov export of each test is still one `llvm-cov` process (about
  0.4 s). A candidate: read the profiles and the coverage mapping in process.
