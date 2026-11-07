# layr retest: run only the tests that a change can affect

Status: first version built (2026-10-28): the Rust adapter, and the vitest, jest and pytest adapters (not tried with the real tools), in `crates/layr-retest`. Design
agreed with the owner on 2026-10-28. Prototype measurements are in the last section. See
"First version" for the command line, the record format and what is not done.

## Start

```sh
layr init <project> --from <folder>     # or: layr git clone <url> <project>
cd <project>/lines/main
layr retest init                        # then save the files it names: layr add ... && layr save -m "retest init"
layr retest gate -- <your gate command>
```

## Goal

A test that passed must not run again while the code it executed stays the same, in any line.
The goal is speed only. A saved pass is not evidence for a merge, a release or merge protection;
those keep using `check` attestations of layr (`docs/checks.md`).

A wrong saved pass skips a test that would fail, so it hides a defect. A periodic full run is the
safety net: `layr retest run --full` runs every test and checks the saved passes (see Contradiction).
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

Building only the crates that have selected tests is not done (tried 2026-10-31, dropped). A change in
crate A can break a dependent crate B that has no selected test, and only a full build finds it.
With `-p`, cargo also unifies features over the selected packages only: in Studio this rebuilt
110 crates into a second artifact set (+11 GB) and the first run gained no time. Incremental
compilation (below) gives the gain without these rules. The build with selected tests stays one
build of the whole selection of the nextest arguments.

## Incremental compilation

`layr-retest` sets `CARGO_INCREMENTAL=1` for its own cargo calls. They write to `target/retest`
only, so this does not change the caller's builds. It is set by the tool and not by the caller
because the callers (a gate script) set `CARGO_INCREMENTAL=0` for their plain builds, and a second
variable per caller would be easy to forget. `LAYR_RETEST_INCREMENTAL=0` leaves the caller's value.
The first incremental build of a crate is slower than a non-incremental one; the gain is on later builds.
A rustc wrapper such as sccache (set in `~/.cargo/config.toml`) refuses incremental compilation, so the tool also
clears `RUSTC_WRAPPER` for those calls. The gate does the same for its command.

### Measurements 2026-10-31

Studio line `retest-gate`, `--workspace --profile fast`, change: `black_box(N)` in `spawn_process`
(6 tests selected), `CARGO_BUILD_JOBS=4`, `nice -n 10`, one run per gate-lock take. Load is the
1-minute average at the start (other agents ran). Times are seconds; "total" is build and list +
run and export + record, without save and diff (APFS cost, out of scope).

| Run | Load | Build and list | Run and export | Total |
| --- | --- | --- | --- | --- |
| Baseline (`CARGO_INCREMENTAL=0`) | 26 | 237.7 | 52 | 289.7 |
| Incremental, first run (fills the cache) | 11 | 250.8 | 37 | 287.9 |
| Incremental, second run | 11 | 68.4 | 29 | 97.5 |

The baseline ran 12 tests (older baseline state); both incremental runs ran the same 6.
`debug/incremental` was 10.9 GB after the first incremental run and 16.4 GB after the second (`du`).

Probes: `panic!("probe")` in `layr_model::diff::is_binary` (layr workspace, `--workspace --exclude
layr-retest`, 262 tests): the incremental run selected 2 tests and failed the same 2 `interdiff`
tests as `--full` (260 passed). A `panic!` in the Studio supervisor `main.rs` (bin source that no
record has) forces a full run and failed the same 20 tests as plain nextest.

## Debug info level

`layr-retest` sets `CARGO_PROFILE_DEV_DEBUG=line-tables-only` and `CARGO_PROFILE_TEST_DEBUG=line-tables-only`
for its own cargo calls (nextest builds with the test profile, which inherits from dev; both are set
so that a `[profile.test]` of the project does not win). Coverage needs the coverage mapping and
the line tables, not variable info. The variables apply to `target/retest` only. The tool sets them
and not the caller, for the same reason as `CARGO_INCREMENTAL`. `LAYR_RETEST_LINE_TABLES=0` leaves
the project's own level.

## Profile files and build scripts

Without `--target`, cargo applies `RUSTFLAGS` (and the project rustflags) to build scripts too, so
build scripts are instrumented. A build script that runs without `LLVM_PROFILE_FILE` writes
`default_<hash>_0_<pid>.profraw` into its package folder, which is the project. The leak came from
the sweep listings (`cargo build --features ...` of a standard command): they compile and run a build
script that the test run did not build, and they had no `LLVM_PROFILE_FILE`. Reproduced 2026-11-02:
a `sweep` command with a feature and a `build.rs` gave `removed 1 stray .profraw file(s)` before the
fix, and none after.

Rules (`src/main.rs`, `profile_scratch` and `cargo_profile_file`):

- Every cargo call of the instrumented build sets `LLVM_PROFILE_FILE` to
  `target/retest/profraw/other/%p.profraw`: the list phase, the build, the run and the sweep listings.
  The wrapper still sets its own value for each test.
- The scratch folder `target/retest/profraw` is deleted when a run ends, on every exit (a guard), and
  after each sweep listing.
- The stray cleanup at the end of a run stays. It only catches a child that clears its environment.

Build scripts stay instrumented. Measured:

- A `[host] rustflags` value in a config file did not reach the build script or the bin of a probe
  package on stable cargo 1.99.1. The build script got `-C instrument-coverage` from `RUSTFLAGS`
  and no host flag. So config does not keep the flags off build scripts.
- Cost: `cargo build` of a one-file package with an empty build script, 0.45 to 0.61 s without the
  flags and 0.54 to 0.69 s with them (two pairs, one build script rebuild each). About 0.08 s per
  build script rebuild. Records never use build scripts, so nothing is lost.
- Not tried: `--target <host>` keeps the flags off build scripts, but it moves every output folder
  that the tool reads (`target/retest/debug`), so it was not changed.

## Sweeping stale build output

Cargo never removes the output of an old unit. A unit is one compiled crate, test binary or build
script run. Its name ends in a 16 hex digit hash (`name-<hash>`). A new feature set, a changed flag,
a new toolchain or a focused command leaves the files of the units that no later build uses.

`layr retest sweep [--dry-run]` removes them. `layr retest run` sweeps after its build with the same rule.
`layr retest sweep` never compiles: it stops at the first unit that is not fresh and skips that folder (see
"Freshness").

### Live set

The live set is the set of unit hashes that the standard build commands use in this tree. The commands
are in the root `Cargo.toml` of the workspace:

```toml
[workspace.metadata.layr-retest]
env = { RUSTUP_TOOLCHAIN = "1.99.1-aarch64-apple-darwin", RUSTC_WRAPPER = "", CARGO_INCREMENTAL = "0" }
sweep = [["build", "--workspace", "--exclude", "studio-native-client", "--tests", "--bins", "--examples", "--locked"]]
retest = [["test", "--workspace", "--no-run"]]
```

- `env` holds strings. A table named `macos` or `linux` in it applies on that OS only, over the plain values, for
  paths that differ by OS: `env = { CARGO_INCREMENTAL = "1", macos = { CC = "<clang>", SDKROOT = "<sdk>" } }`. The
  cache key, the prune listing and retest read the same table.
- `sweep` lists cargo commands without `cargo`. It runs each command in `target` with `env`, as a listing
  (see "Freshness"). The listing adds `-v --message-format json`. A command whose units are not all fresh
  skips the folder; the listing never builds a unit that is not fresh.
- An entry of `sweep` can be an object for a test that builds into a folder of its own below `target`:
  `{ target_dir = "provider-contract-compile", args = ["check", "--offline", "-p", "x"], env = { RUSTFLAGS = "" } }`
  (a TOML inline table in the `sweep` array). The follow pass builds it with `CARGO_TARGET_DIR=target/<target_dir>` and the project `env`
  plus its own `env`, and the prune of `save --caches` keeps the units of that command there and keeps the folder
  however old it is. `retest` does not run these entries.
- `retest` (optional) lists the commands of the retest build. `layr retest sweep` sweeps `target/retest` with
  `sweep` and `retest` under the environment of `run`. Without `retest`, `target/retest` is left as it is and the
  sweep prints `target/retest: no sweep command; left as is`. The sweep never creates `target/retest`.
- The live set comes from cargo's messages: every artifact and every build script run. A bin or an example
  is named only by its copy (`debug/app`); the sweep matches the copy to its hashed file by size and
  modification time. A library with an unhashed output is live with its crate (see "What goes").
- `env` sets the variables that change the unit hashes (toolchain, wrapper, incremental). It is applied
  over the environment of the caller.
- `layr retest run` adds its own command (`test --no-run` and its arguments) to the list. A focused run
  does not remove the units of the standard commands, and the units it built stay.
- Without a `sweep` list, `layr retest run` skips the sweep and `layr retest sweep` prints a skip. Nothing is
  removed, and both exit 0.
- `layr retest run` sweeps `target/retest` with the `sweep` and `retest` lists and its own command, under the
  environment of `run` (instrumented RUSTFLAGS, debug info, incremental). The run needs no `retest` list.

### What goes

The sweep works on an allowlist. Every entry of a swept profile folder (`target/<...>/<profile>`) that the
live set does not name goes. The live set names:

- each output that cargo lists (`filenames`, `executable`, the `out_dir` of a build script), by its path
  in the profile folder;
- each live unit hash: its entries in `.fingerprint` and `build` (with `out/`), and its files and folders
  with that hash in `deps` and `examples` (`.rlib`, `.rmeta`, `.d`, object files, test binaries, `.dSYM`);
- each live crate with unhashed outputs (`deps/libcrate.rlib`, `libcrate.dylib`): the newest unit of that
  crate in `.fingerprint` (the marker `lib-<crate>`, newest `invoked.timestamp`), with its files `lib<crate>.*`
  and `<crate>.d` in `deps`;
- the newest object build of each live owner (a unit hash, or an unhashed crate);
- the dep-info `<output>.d` of a listed output in the profile folder;
- cargo's lock files (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`);
- each `incremental/<crate>-<id>` folder that a kept output is hard-linked to, with its newest finalized session
  and that session's lock file. For a crate with a live test binary, bin or example and no link, the newest such
  folder (see "Other profiles"). rustc names the lock `s-<time>-<id>.lock`, the session name without its hash.

Everything else goes: uplifted copies that no listing names, stale bins and examples, unhashed objects of
other builds, outputs of units that no command lists, orphan incremental folders, and other files. Entries
go whole, so no live unit loses a file. The removal frees the bytes of an inode only when all its links go.

Folders of `target` that no `sweep` command makes have no live set: another `--target` triple
(`target/<triple>`), a custom profile (`target/<name>`), `target/doc`. The cache prune (`save --caches`, the
follow pass) removes such a folder when no build wrote into it for 7 days: the newest time of the folder, its
entries and their entries is older (cargo writes `.cargo-lock` and `.fingerprint` there at each build). The profile
folders and `--target` triples of the `sweep` commands, `target/retest` and `target/profraw` stay, as do files at
the top of `target` (issue #195). A project that uses such a build every week lists its command in `sweep`.

Links from a unit to its incremental folder (both need no compile):

- Library: the newest session of a library holds `metadata.rmeta`, hard-linked to `deps/lib<crate>-<hash>.rmeta`
  (link count 2). The sweep keeps the folder through that link, so a library built without debug info keeps its
  folder. Measured 2026-11-03 on a test workspace with `debug = 0`.
- Object files: rustc hard-links the object files of a session to the outputs in `deps`. This needs debug info.

The folder name carries rustc's stable crate id (base-36), not the cargo hash, and no text file names it.

Lock: the sweep keeps the lock of the kept session. Without it rustc cannot reuse the object files of that session:
measured 2026-11-03, a one-line change after a sweep rebuilt both units of a library with debug info off, and with the
lock put back the unchanged unit was reused (same inode). Test: `sweep_keeps_the_incremental_session_of_a_library_without_debug_info`.

### Other profiles

A profile folder is swept only when a configured command lists an output in it. The profile is named by the
listing, so a project that builds only `target/release` is swept (test: `sweep_sweeps_a_release_only_target`).
A folder is swept when any profile folder in it has a fingerprint folder (a build wrote it). In Studio the
listed profiles are `target/debug` and, with a `retest` list, `target/retest/debug`; `target/wasm32-unknown-unknown`
is not listed, so the sweep leaves it as it is.

Limit: a test binary, bin or example built without debug info has no link to its incremental folder (no object
file, no `metadata.rmeta`). For each crate with a live one of these, the sweep keeps only the newest unlinked folder of
that crate and removes the others, so at most one such folder per crate stays. Two trade-offs:

- A bin and a test binary of the same crate, both live and both without debug info, share that one folder. One of them
  recompiles after a sweep.
- A non-live build of the same crate that ran last (for example a focused command with other features) can be the
  newest folder. Then the live folder is removed, and the next standard build recompiles once.

Set `debug = 1` for that profile to keep the object link. The layr project sets `[profile.release] debug = 1`.

A `studio-native-client` output (for example `deps/libstudio_native_client.a`) is not listed, because the sweep
list excludes that crate, so the sweep removes it. A build that needs it compiles it again.

Known limit: the sweep needs every listed command to be fresh. A command with a stale unit makes the sweep skip
that folder (see "Freshness"), so run the standard build before the sweep.

### Object files

Each build writes a new set of object files (`<crate>-<hash>.<cgu>.<build>.rcgu.o`, and for unhashed
crates `<crate>.<cgu>.<build>.rcgu.o`) for the same unit. A source change keeps the unit hash, so the old
sets pile up. Cargo does not use them. For each live owner, only the files of its newest build stay (newest
modification time). Two name forms carry a build id: `<crate>-<hash>.<cgu>.<build>.rcgu.o`, and
`<crate>-<hash>.<crate>.<id>-cgu.<n>.rcgu.o` (one `<id>` for all codegen units of a compile; the Studio crates use
this form). A name with no build id is not an object of a known build and goes with the rest of the allowlist.
Test: `sweep_keeps_the_newest_object_build_of_each_live_owner`.

### Freshness

The sweep never compiles. Before it removes anything, each listing checks that every unit of its commands
is fresh, with no build:

- The listing runs `cargo <command> -v --message-format json` with `CARGO_BUILD_JOBS=1`, in its own process
  group. The status lines of `-v` name a unit before cargo acts on it: `Dirty <package>: <reason>` for a unit
  that is not fresh, and `Compiling` or `Running` for a build step. The first such line stops the listing:
  the group is killed.
- A `Blocking ... directory` line (another cargo holds the lock of the folder) also stops the listing. The
  listing never waits for that cargo. A wait for the shared package cache is short and does not stop it.
- A JSON `compiler-artifact` with `fresh: false` also stops it. It comes after cargo has built that unit,
  so it is a fallback for a cargo whose status text differs. Then one unit is built.
- A stopped folder is skipped with `sweep skipped: a unit is not fresh (<status line>); run the standard
  build first`. The other folder is still swept. The exit code is 0.
- Measured on 2026-11-02 with cargo 1.99.1, a two-crate test workspace: a stop at 0.11 to 0.18 s. A fresh
  listing of the same workspace: 0.1 s. A tree of Studio's size was not measured.

What a stopped listing can still change:

- Cargo removes the previous output of the first stale unit before it runs that unit's build step. The stop
  can come after that removal (measured: the `.rlib` of a stale crate was removed in every run). The next build
  compiles that unit anyway, because it was stale. No new output file is written.
- Cargo writes its fingerprint marker files, and it creates the fingerprint folder of a unit that was never built.
- A listing that names an uplifted binary (`debug/<bin>`) re-links it to the unit of the command. A standard
  `cargo build` does the same.

### Safety

- Every target is listed before any removal. A failing command in any target changes nothing.
- A folder that is not fresh is not swept. The next build of its commands makes it fresh, and then the sweep
  removes its stale units.
- The removal holds cargo's build lock (`debug/.cargo-lock`) and does not wait for it. A busy lock skips that
  folder. A listing that ran before the lock can lose the newest units of a cargo that finishes between the two;
  the next build compiles them again.
- Exit codes: 0 for a skip (no `sweep` list, no `retest` list, a stale or busy folder), 1 for a failing cargo
  command, 2 for a usage error.
- An empty live set removes nothing.
- `--dry-run` prints the bytes that a sweep would free and removes nothing.
- A dylib used as a dev-dependency has an unhashed output (`libdy.dylib`) and unhashed objects. Its
  fingerprint is the newest `lib-<crate>` unit, and its incremental folder is the one linked to the newest
  objects. A feature build of the dylib leaves a second folder that the sweep removes. Tests:
  `sweep_keeps_the_incremental_folder_of_a_dylib_dev_dependency` (e2e) and
  `plan_names_the_outputs_of_the_messages_per_profile` (unit).
- Output: `swept <folder>, freed N MB`, and the time of the whole command.

### Listing of a command that was never built

A command whose units are not built yet (for example `--examples` when the test build did not compile them)
is not fresh. The `run` sweep and `layr retest sweep` skip the folder, as in "Freshness".

### Studio measurement (2026-11-03, sweep allowlist)

Throwaway line from origin/main (`8dafcde`) of Studio, gate environment (`CARGO_BUILD_JOBS=8`,
`CARGO_INCREMENTAL=1`, no wrapper, toolchain 1.99.1), gate build
`cargo build --workspace --exclude studio-native-client --tests --bins --examples --locked`. The line started
from a warm `target` (24.6 GB by `du`).

| Step | Crates compiled | `du -sk target` | Sweep |
| --- | --- | --- | --- |
| Gate build | 0 | 24,645,448 KB | |
| Dry run | | | would free 2733.6 MB |
| Sweep 1 | | 21,974,188 KB | freed 2733.6 MB |
| Build after sweep 1 | 0 | | |
| One-line change (comment) in `request-signing`, build | 12, 187 s | 26,444,172 KB | |
| Sweep 2 | | 22,211,160 KB | freed 4320.3 MB |
| Build after sweep 2 | 0 | | |

Sweep 1 by kind (unique inodes, MiB, one name per file):

- top-level files, 33 files, 1768.4: the four runtime rlib copies (`libstudio_runtime_services.rlib` 857.6,
  `libstudio_runtime_http.rlib` 506.0, `libstudio_runtime_store.rlib` 152.1, `libstudio_runtime_schema.rlib` 86.8),
  `studio-native-client` 76.1, and smaller uplifted copies;
- `deps`, unhashed other, 7 files, 380.4: `libstudio_native_client.a` 310.5 and `.rlib` 67.8;
- `incremental`, 755 files, 358.8: orphan crate folders;
- `deps`, unhashed objects, 92 files, 99.1;
- `examples`, unhashed other, 4 files, 0.3.

Total 2606.9 MiB, equal to the dry run (2733.6 MB). Hashed `deps` counts did not change (1762 files), so the
earlier sweep had already removed the dead hashed units. The unexplained bytes are the unhashed and uplifted
files. The four runtime rlib copies (about 1.6 GiB) are byte-identical in size and time to their hashed `deps`
rlibs. They are separate inodes, so `du` counts them, and no cargo listing names them. The writer is not
identified. The sweep removes them, and the next build compiles nothing.

Sweep 2 by kind (unique inodes, MiB): `deps` hashed 5919 files 4079.0 (older object builds of the 12
rebuilt units), `deps` unhashed objects 256 files 22.5, `examples` hashed 454 files 15.3, incremental 108 files 3.1.
The build of the change wrote about 4.5 GB of new object files; the sweep removed the older builds. The net growth
of the change is 231 MiB.

### Studio measurement (2026-11-01)

Studio `sweep-check` line from origin/main (`target` of about 53 GB apparent size, `du`). Gate build:
`cargo build --workspace --exclude studio-native-client --tests --bins --examples --locked` with
`RUSTUP_TOOLCHAIN=1.99.1-aarch64-apple-darwin RUSTC_WRAPPER= CARGO_INCREMENTAL=0`.

| Step | Result |
| --- | --- |
| Gate build after the base change | 6 crates compiled, 142 s |
| Dry run, first after the #114 rustflags change | retest folder listing compiled 535 units (702 s), then the target sweep freed 0 MB |
| Sweep (warm) | 2.6 s: `target` freed 0 MB, `target/retest` freed 13,247 MB |
| Build after sweep | 0 crates compiled |
| One-line change, build | 6 crates compiled, 162 s; `target/debug` du +54 MB, no new object sets |
| Dry run before the change sweep | retest listing compiled 22 units (170 s), so that folder was not swept in the dry run |
| Sweep after the change (warm) | 1.4 s, freed 0 MB; `target/debug` du back to the size before the change |
| Build after the change sweep | 0 crates compiled |

The timed sweeps are the warm ones. The long times came from cargo's listing compiling a folder that was
stale, which the sweep then skips. Freed sizes count files with one hard link; APFS clones are not
counted, so `du` changes differ from the freed value.

### Measurements 2026-10-31 (debug info and sweep)

layr workspace, `--workspace --exclude layr-retest`, 262 tests, `CARGO_BUILD_JOBS=4`, `nice -n 10`,
`--full` for the first rows. Load average 8 to 19 (other agents ran), so times are only indicative.

| Build (cold `target/retest`, `--full`) | `target/retest` | deps | incremental | Time |
| --- | --- | --- | --- | --- |
| default debug info (installed tool) | 2.3 GB | 1.6 GB | 0.4 GB | 129 s |
| `line-tables-only` | 1.9 GB | 1.2 GB | 0.4 GB | 161 s, 126 s (second run) |

Records of the `--full` runs: all 262 tests have the same status, files and deps in the installed
tool's record and in the new one. The coverage ranges equal in 254 tests. Known run-to-run variation:
`store_remote::writer::tests::pending_part_age_survives_double_crash_and_gc_before_publication`
differs between two runs of the same binary, and `...fresh_push_discards_wal_sidecars...` and
`thin::tests::readonly_directory_metadata_keeps_modes` differ between runs with the same debug level.
With `line-tables-only`, six `layr-model::index` tests have one more `idle` range per file (an
uncalled function, for example `lib.rs` 35 to 54), the same in both runs of the new tool; with
`LAYR_RETEST_LINE_TABLES=0` the records equal the installed tool's apart from the known variation.
The extra range is a function that no test calls, so no selection depends on it.

Sweep: three small changes (`min(8000)` to `min(8000 + 0)`, then more `+ 0`), 2 tests selected each
time.

| Step | `target/retest` | deps |
| --- | --- | --- |
| After `--full` (no object file sweep yet) | 2836 MB | 1273 MB |
| After change 1, 2, 3 without the object sweep | 2982, 3129, 3275 MB | 1405, 1537, 1669 MB |
| Sweep on the next run (change 4) | 2766 MB (freed 696 MB) | 1131 MB |
| After change 5, 6, 7 with the sweep | 2778, 2793, 2806 MB | 1130, 1131, 1131 MB |

Without the sweep the folder grew by 146 MB per small change; with it by 12 to 15 MB (incremental
folder). The first sweep after a toolchain or debug level change freed 1251 MB (the old build).

Probe: `panic!("probe")` as first statement of `layr_model::diff::is_binary`. The incremental run
selected 2 tests and failed 2; `--full` ran 262 and failed the same 2
(`interdiff::tests::patch_format_keeps_modes_binary_renames_and_missing_newlines` and
`interdiff::tests::same_patch_has_no_interdiff_and_amendment_has_file_counts`).

### Time cost (2026-10-31, same workspace, load 9 to 16 at the start of each run)

A disk change must not make a run slower. `LAYR_RETEST_SWEEP=0` switches the sweep off for the comparison.

| Measure | Result |
| --- | --- |
| Extra `cargo test --no-run --message-format json` (all fresh) | 0.33 and 0.36 s (0.78 s at load 40) |
| Sweep itself, about 455 MB freed | 0.27 and 0.30 s (1.87 s once, at load 40, 467 MB) |
| Sweep, total per run | 0.6 to 0.7 s, printed in the `swept ...` line |
| Build and list, 3 small changes, `line-tables-only` | 6.9, 5.9, 11.4 s |
| Build and list, 3 small changes, `LAYR_RETEST_LINE_TABLES=0` | 8.4, 5.9, 12.9 s |
| Cold `--full`, `line-tables-only` / default debug info | build 57.5 s, run 129 s / build 90.8 s, run 201 s |
| `target/retest` after the cold run and 3 small changes | 2257 MB / 2603 MB |

Line tables are not slower to rebuild (equal within the load noise) and the cold build is faster.
The sweep stays under 1 s, so it runs on every build and keeps no state. Sweep-on and sweep-off
runs of the whole tool (`sweep-on`: 12.4 to 36 s build and list, `sweep-off`: 30 to 104 s) cannot be
compared: other agents raised the load to 30 to 130 during the sweep-off runs. Use the printed
times of the artifact list and the sweep.

### Unique disk of a new line (APFS clones)

`du` counts clones in full, so these use the change of `df` free space. A new line is a `cp -c -R`
of `target/retest`; then one small change is built into the clone (`cargo nextest run --no-run
--workspace`, same flags as layr-retest). The clone itself costs about 0.

Layr workspace (`debug/incremental` 0.67 to 0.96 GB, `target/retest` 2.26 to 2.55 GB; two passes, the
second is shown, the first in brackets; free space 52.9 GB before, 50.6 GB after):

| Phase | Change | Build s | Unique GB | Of incremental | Of `target/retest` |
| --- | --- | --- | --- | --- | --- |
| Incremental on | 1 | 18 [16] | 0.97 [0.86] | 1.01 [1.28] | 0.38 [0.38] |
| Incremental on | 2 | 7 [6] | 0.26 [0.32] | 0.28 [0.48] | 0.10 [0.14] |
| Incremental off | 1 | 39 | 1.26 | 1.31 | 0.49 |
| Incremental off | 2 | 32 | 0.67 | 0.70 | 0.27 |

Studio (`debug/incremental` 9.38 GB, `target/retest` 34.3 GB by `du`), change 1 only, incremental on:
build 147 s, unique 15.84 GB (1.69 of incremental, 0.46 of `target/retest`). This is a `df` delta
and other agents wrote to the disk meanwhile. Change 2 and the incremental-off phase are not
measured: the run stopped at the 15 GB free-space floor.

Reading: the clone is free; each change costs the files it rewrites. With incremental on a second
change in a line is cheaper (0.26 GB against 0.67 GB off in the layr workspace) and much faster
(7 s against 32 s). The first change in a new line costs about the size of the incremental
directory (layr 1.0, Studio 1.7 times).

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

1. Build the tests with `-C instrument-coverage` added to the project rustflags (see "Project
   rustflags") in a separate target folder.
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

### Project rustflags

Cargo reads the rustflags from the first set source of: `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`,
`target.<triple>.rustflags` (config files, `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`), `build.rustflags`
(config files, `CARGO_BUILD_RUSTFLAGS`). Measured with cargo 1.99.1: a probe workspace whose
`RUSTC_WRAPPER` logged the flags that rustc received.

| Setting (config has `build.rustflags = ["--cfg", "projflag"]` unless noted) | rustc receives |
| --- | --- |
| `RUSTFLAGS=-C instrument-coverage` | only the coverage flag: the config value is dropped |
| `RUSTFLAGS=""` | nothing: an empty variable also replaces the config |
| `CARGO_ENCODED_RUSTFLAGS` (with `RUSTFLAGS` set too) | the encoded value only: it wins over `RUSTFLAGS` |
| `CARGO_BUILD_RUSTFLAGS=-C instrument-coverage` | config and variable merged |
| `--config 'build.rustflags=[...]'` | config and option merged |
| `target.<triple>.rustflags` in config, `build.rustflags` too | the target value only: build is ignored |
| `CARGO_BUILD_RUSTFLAGS` with a target value in config | the target value only |
| `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` with a target value in config | config and variable merged |
| `target.'cfg(...)'.rustflags` | it shadows `build.rustflags` like a target value |
| target value in the parent folder, build value in the line | the parent target value only |

Two sources have two sections: a target value hides the build value. So one variable cannot add
the flag to every project. `cargo config get` is nightly only (not usable on 1.99.1). A
`RUSTC_WRAPPER` that adds the flag would replace the `sccache` wrapper of `~/.cargo/config.toml`.

Choice: `layr retest` reads the config files that cargo reads for the line (`$CARGO_HOME`, then
`.cargo/config.toml` and `.cargo/config` of the line and its parents, top down). It sets only the
variable that cargo reads:

1. `CARGO_ENCODED_RUSTFLAGS` if set: the value plus the flag.
2. Else `RUSTFLAGS` if set: the value plus the flag.
3. Else `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` if a `target.<triple>` value exists in a file or the
   variable is set.
4. Else `CARGO_BUILD_RUSTFLAGS`.

`<triple>` is `CARGO_BUILD_TARGET`, else `build.target`, else the host. Retest never sets
`RUSTFLAGS` itself, so a project value is never replaced. Limits (retest stops with an error):
a `target.'cfg(...)'` value with rustflags, a `build.target` that is not a string.
Tests: unit tests in `src/rustflags.rs`; e2e `build_rustflags_reach_the_instrumented_build` and
`target_rustflags_reach_the_instrumented_build` (a project whose only `cfg(projflag)` comes from
its config; both fail with the old `RUSTFLAGS` code).

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

A separate crate `layr-retest` in this repository, a library with the entry point `layr_retest::main(args)`.
The `layr` binary dispatches `layr retest <sub>` to it and has no other code for tests, coverage or adapters.
The crate uses only the public CLI of layr, as processes of the running `layr` executable. It does not
call the internal API: a process call keeps the crate free of layr internals and was already tested.

## Prototype measurements (2026-10-28, this Mac, Rust 1.99.1, nextest 0.9.146)

- The cargo target `runner` receives `NEXTEST_TEST_NAME` and `NEXTEST_BINARY_ID`; each test wrote
  its own profile. The nextest wrapper-scripts feature is not needed (it is experimental).
- `layr-model` and `layr-store`: 124 tests, 2.0 s run, 43 s cold instrumented build.
- Raw profiles: 1.5 MB per test (temporary). Reduced records (executed line ranges in project
  files): 34 KB for 116 tests, 5.8 KB after gzip.
- Coverage export: 0.5 s of CPU per test with the binary of that test only (1.8 s with all
  binaries). This is the main cost; it runs only for the tests that ran and is parallel.

## Measurements on the layr workspace (2026-10-28, this Mac, state a5310ad)

`layr retest run -- --workspace --exclude layr-retest`, 261 tests, 4 test threads, 4 build jobs.
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
- A workspace `dylib` target (a crate with `crate-type = ["dylib"]` that holds third-party
  dependencies, so that test binaries stay small) is found with `cargo metadata`. Every built dylib in `target/retest/debug` is an `-object` of every test, because it
  runs inside the test process. The build does not add `-C prefer-dynamic`, so instrumented binaries link std like the plain build and can be copied and run elsewhere. Crates that bundle C libraries must stay out of the dylib.
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

Crate `crates/layr-retest`, a library. The `layr` binary depends on it and runs `layr retest <sub>` in the process of the caller (not in the daemon). There is no separate binary. It runs the `layr` CLI (as a process of `std::env::current_exe()`), `cargo nextest`, `llvm-profdata`
and `llvm-cov` as programs and uses no internal crate of layr. The two LLVM tools come from the
`llvm-tools` rustup component of the active toolchain.

### Command line

Run in the root folder of a layr line that is also the cargo project root.

- `layr retest run [--full] [--save] [-- <nextest args>]`
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
  5. `cargo nextest run --no-fail-fast --message-format libtest-json -E '<filter of the stale tests>'`
     runs them. A run of all tests has no filter. The tool reads the event stream of nextest.
  6. When an event says that a test finished, the tool exports the coverage of that test while the
     other tests still run: `llvm-profdata merge -sparse`, then `llvm-cov export` (lcov for
     executed lines) on the binary of that test, and `llvm-profdata show --covered --all-functions`
     for the executed function names. Function ranges come from a table that is built once per
     test binary. Four exports run at a time (`EXPORT_WORKERS`). Files outside the project, in `target/`, and in `.cargo`, `.rustup` and `/rustc/` are dropped.
     The wrapper merges and deletes the raw profiles of a test right after the test; the export deletes the rest (see "Raw profile disk").
  7. The record is written.
  8. The exit code is the exit code of nextest. The last line prints how many tests ran, passed,
     failed and were skipped.
  Before step 4 the fast path of "No build when nothing can change" can end the run.
  Every step prints `layr-retest: step <name>: <seconds>s`.
  The build of `run` uses the project env (see `gate`). The nextest arguments after `--` go to both `list` and `run`. Use them to select packages
  (`-p`, `--workspace`). Do not put `-E` there.
- `layr retest wrap -- <test binary> <args>` is the cargo target runner that `run` installs
  (`CARGO_TARGET_<TRIPLE>_RUNNER`, set to `<layr> retest wrap --`). With `NEXTEST_TEST_NAME` set it sets `LLVM_PROFILE_FILE` to
  `$LAYR_RETEST_PROFILE_DIR/<binary id>/<test>.profraw` (names cleaned to letters, digits and
  `_-.`), then runs the test, merges its raw profiles and returns its exit code (see "Raw profile disk"). In the list phase it sets no profile and execs the test.

- `layr retest gate [--name <n>] [--max-load <n>] -- <command...>` runs a gate command the same way for every project.
  It replaces a project wrapper script. Steps:
  1. Lock: the folder `/tmp/layr-retest-gate.lock` (files `pid` and `owner`), taken in one step, so one gate runs
     at a time on the machine. A waiting gate prints the pid and the owner of the holder. A lock whose pid is
     not alive is removed (a crashed gate). `LAYR_RETEST_GATE_LOCK` names another path. Ctrl-C does not remove
     the lock; the next gate does. Two gates that reap the same dead lock at the same instant can both win.
  2. Load: waits while the 1-minute load is at or above `--max-load` (default 60), at most 10 minutes. Then it starts.
  3. Env: `[workspace.metadata.layr-retest] env` (none without `Cargo.toml`). The command runs with it, with
     `RUSTC_WRAPPER` empty and `CARGO_TARGET_DIR` removed.
  4. `cargo fetch --locked` in a cargo project. A failure prints a warning and the gate goes on.
  5. The command. Its output goes to the terminal. In a cargo project a `cargo` script comes first on PATH: it
     records each top-level cargo call (its folder, `CARGO_TARGET_DIR`, the variables of the unit env, its
     arguments) and runs the real cargo. A call from inside cargo (cargo sets `CARGO` for build scripts,
     `cargo-nextest` and `cargo-clippy`) is not recorded; `CARGO` from the gate's own caller is removed for the
     command. A tool that runs cargo by an absolute path is not recorded.
  6. Gate coverage, after a command that passed (see "Gate coverage" below).
  7. `layr retest sweep` in a cargo project. The last line says `sweep ok`, `sweep skipped` or `sweep failed`.
     A failed sweep does not change the gate's exit status.
  8. The lock is released. The exit status is the status of the command.
- `layr retest init [--yes]` makes a project ready. It is safe to run twice.
  - Finds a cargo project (`Cargo.toml`), pnpm (`pnpm-lock.yaml`), and vitest, jest or pytest projects.
  - Adds a `regenerable` line to `.layrattributes` for each cache folder of the project that it finds: `target` (cargo),
    `node_modules` (pnpm) and `.layr-retest` (records of cargo, vitest, jest or pytest). Each line is added once.
  - Adds `.layr-retest` to `.gitignore` when the project has a test tool.
  - Adds the keys `sweep` (`build --workspace --all-targets --locked`) and `env` (`CARGO_INCREMENTAL = "1"`, `RUSTC_WRAPPER = ""`) to
    `[workspace.metadata.layr-retest]` in `Cargo.toml` when they are missing. A package-only manifest gets a
    `[workspace]` table, so that cargo reads the metadata. A key that the project set keeps its value.
  - Saves each cache folder that exists and is not a cache overlay yet: `layr save --caches --cache <folder>`.
    It does nothing for a folder that is already an overlay. Outside a layr line the save fails, and the exit code is 1.
  - Checks `cargo-nextest` and the `llvm-tools` rustup component of the active toolchain. A missing one
    prints its install command (`cargo install cargo-nextest --locked`, `rustup component add llvm-tools`);
    `--yes` runs it. Exit code 1 if one is still missing.
  - In the main line of a project with a Git remote `origin`, sets `layr follow origin/main` (once) and runs its
    first pass. Each pass builds the `sweep` commands in main and saves its caches; a line's caches go back to main
    after `layr git push` of the line or a local `layr merge`. Without a running daemon it says how to install one.
  - Prints the next step: `layr retest gate -- <your gate command>`.
- `layr retest doctor` reads and never writes. It prints `ok:` or `warn:` lines with numbers; exit code 0
  when there is no warning, else 1.
  - Tools present, and the LLVM major version of `llvm-profdata` equals that of `rustc`.
  - Test sources (files under `tests/`, and code after `#[cfg(test)]`) that start a bin by a
    `target/debug` path or by an env variable whose name has a word `BIN`, `BINARY`, `EXE` or `EXECUTABLE`, or is a bin target name.
    That bin is not instrumented, so a change of it forces full runs. Use `env!("CARGO_BIN_EXE_<name>")`.
  - The number of test targets (lib, bin, integration), the size of the integration test binaries (from
    `cargo metadata` and the files in the `deps` folder of every profile folder: `target/<profile>`,
    `target/<triple>/<profile>` and `target/retest/<profile>`; it does not run
    `cargo nextest list`, which builds). For a crate with two or more integration test binaries: one
    binary (`autotests = false`, one `tests/main.rs`) saves the size of all but the largest and one link each.
  - Build script output (`build/*/output` in every profile folder: `target/<profile>`, `target/<triple>/<profile>`
    and `target/retest/<profile>`):
    - A `cargo:rerun-if-changed=` (or `cargo::`) value that is an absolute path under the project root is a
      warning. Each line has its own absolute path, so cargo marks the build script dirty in every new
      line and rebuilds all crates after it, although `target/` is warm. Print a path relative to
      `CARGO_MANIFEST_DIR`.
    - A value with a `.git` component, relative or absolute (for example `../../.git/HEAD`), is a warning.
      Each line has its own `.git`, so the same rebuild happens. It prints the fix: take the version from
      the build environment (for example an env var set by the release build) instead of watching `.git`.
    - Without build output, it says `build once to check build script paths`.
  - Records: count, and the age of the newest (warning above 14 days). Size of `target/retest`
    (warning above 20 GB).
  - Units that name a file out of their package by an absolute path (the binary dep-info in `.fingerprint`):
    an `include_str!`, `include_bytes!` or `#[path]` of a file in another folder of the workspace. Cargo names a
    source of the unit's package relative to the package and a source in `target` relative to `target`; any
    other source by its absolute path. A copy of the workspace at another path builds such a crate again, and a
    new line keeps it warm only when each file has the same bytes there (`relocate`). One warning per package,
    with the number of files and the first three. The workspace manifest, `Cargo.lock` and the clippy config are
    left out: cargo and clippy name them so in every unit. Fix: move a shared file into the package, or copy it
    to `OUT_DIR` in `build.rs` and include it from there.
  - The coverage of the last gate (`.layr-retest/gate-coverage.json`): each call that sweep does not cover is a
    warning, with the age of the gate. Without a gate it says how to check one.

### Gate coverage

A new line is warm only for the units that main builds, and main builds the `sweep` commands. A gate whose cargo
calls use other units builds them cold in every new line. After a passing command the gate lists each recorded call
the way the sweep lists its commands: `-v --message-format json`, one job, and a rustc wrapper that never
compiles, so the listing stops at a unit that is not fresh and builds nothing. It runs before the sweep, which
removes the units that no sweep command lists.

- A call is listed with its unit arguments. `test`, `bench` and `cargo nextest run|list|archive` become a
  `--no-run` test build with the same package, target, feature and profile options (nextest's `--cargo-profile`
  is cargo's `--profile`; its own `--profile`, filters and runner options go). `test --doc` becomes a library
  build, `run` a build. Job, color, message format and verbosity options go. A call that builds nothing (`fetch`,
  `metadata`, `fmt`, a help or version call) is left out.
- The target folder is `--target-dir`, else the call's `CARGO_TARGET_DIR`, else `target` of the project. A call
  into `target` is compared with the `sweep` commands, one into `target/<dir>` with the `sweep` entries whose
  `target_dir` is `<dir>`. A call into a folder out of `target` is a warning: no line takes those units warm.
- The units of a call (unit hashes per profile folder) that no `sweep` command lists are named, up to five, with
  the `sweep` entry that covers them: the listing arguments for `target` (and the variables that the `env` table
  needs when the call had another value), or an object with `target_dir`, `args` and `env` for a folder below it.
  A call that selects packages with `-p` gets a note: cargo resolves the features of those packages only, so
  their dependencies get other units than a `--workspace` build.
- Not checked, with the reason: a failed command, a call whose folder is gone (a test copy in a temp folder), a
  call whose units or sweep commands are not fresh after the gate, a busy build lock, more than 40 distinct calls.
- The result goes to `.layr-retest/gate-coverage.json`; `layr retest doctor` prints it.

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
auto state is in no other line. So the flow is: work, `layr save -m`, `layr retest run --save`,
`layr line new X <line>`, and `layr retest run` in X selects only what changed.

The installed `layr` 0.4.0 (2026-10-26) has no cache overlays. Build the one in this repository.

### Raw profile disk

Raw profiles are large (1.5 MB per test in the layr workspace, about 18 MB in Studio). Two steps
keep them small:

- The wrapper (`layr retest wrap`) does not exec the test. It starts the test, waits, and then
  runs `llvm-profdata merge -sparse` on the raw files of that test, writes `merged.profdata`
  (small: only functions with a count above 0) and `raw-count`, and deletes the raw files. It
  returns the exit code of the test. So raw files exist only while their test runs. If the merge
  fails or the wrapper is killed, the raw files stay and the export merges them.
- The export runs while the tests run (event stream of nextest) and deletes the profile folder of
  a test after its export. Finished tests wait for the four export workers with only the small
  `merged.profdata`.

So the raw profiles on disk are those of the tests in flight, and the rest is small. A first
try that only moved the export into the event stream did not bound the disk: the tests ran faster
than four workers could export, and a full nextest pipe does not stop running tests (peak
1059496 KiB before, 930020 KiB after, on the layr workspace).

Measured on the layr workspace (`--full`, `--workspace --exclude layr-retest`, 262 tests,
2026-10-30, load 80 to 135, peak of `du` on `target/retest/profraw` once per second):

| Method | Peak raw profile disk |
| --- | --- |
| Export after all tests (old) | 1059496 KiB and 942044 KiB (two runs) |
| Export in the event stream only | 930020 KiB |
| Merge in the wrapper, export in the stream | 122328 KiB |

The record equals the old record except for tests whose coverage varies from run to run: two runs
of the old method differ in 1 test (`pending_part_age_survives_double_crash_and_gc_before_publication`),
and the new method differs from the old in 2 more `store_remote` tests (one line each, timing
dependent). Probe `panic!("probe")` at the top of `layr_model::diff::is_binary`: the incremental
run selected 3 tests and the same 2 `interdiff` tests failed as in the `--full` run (260 passed).

### Killed child processes

A test can start a bin target of the workspace and stop it with SIGKILL (the Studio supervisor).
LLVM writes a profile only when the process exits, so the killed child writes none, the code of
the bin is in no record, and a change of it selected no test (a `panic!` probe in it: 0 selected,
plain nextest 20 failures). Continuous mode (`%c`) did not work on this Mac (alignment and link
errors). The rule instead:

- After the build, the tool reads the source file of every bin target (`cargo metadata`, targets
  of kind `bin`, field `src_path`).
- A changed file that is the source of a bin target, that exists in the baseline (not a new
  file), and that is in no `files` of any test record of the baseline, selects every test. The
  run prints `bin target source changed and no test record has it: <files>; running every test`
  and the note `full run, changed bin target`.
- Limit: only the file of the target (`src/main.rs`, `src/bin/x.rs`) counts. A module file of the
  bin that only a killed child runs is not seen. A change of a comment-only kind ends in the
  fast path of "No build when nothing can change" and selects nothing, as for other files.

### Processes that a test starts

- The wrapper gives each test a folder `<profile dir>/<binary id>/<test>/` and sets
  `LLVM_PROFILE_FILE` to `<folder>/%p.profraw`. Each process of the test (the test itself and any
  child that keeps the environment) writes its own file. All files of the folder are merged.
- When the folder has more than one profile file, the export uses the test binary and, with
  `-object`, every bin target of the workspace (`cargo metadata`, built in
  `target/retest/debug/<name>`). So code that runs only in a child process is in the record of the
  test that started the child. With one profile file only the test binary is exported: the one
  process was the test itself.
- Every cargo call of the instrumented build has `LLVM_PROFILE_FILE=<profile dir>/other/%p.profraw`, so
  the list phase, the build scripts and the sweep listings write no file in the project (see "Profile
  files and build scripts").
- Limit: a child started with a cleared environment writes `default_*.profraw` in its working
  folder. After the run the tool deletes every `.profraw` file outside `target/` that is newer than
  the start of the run, and prints how many. That coverage is lost: code that only that child ran
  is not in the record, so a change of it does not select the test. `--full` finds such a change.

### JS/TS and Python adapters

`layr retest run --tool vitest|jest|pytest [--full] [-- <tool args>]`. The default tool is
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
- `crates/layr-retest/tests/e2e.rs`: runs `layr retest ...` (the `layr` built from this repository must be first in `PATH`), with a temporary `LAYR_ROOT`, a tiny project with three tests. Runs:
  no record, no change, change of one function, a new uncalled function, a change of a data file,
  full run with a contradiction. About 10 s. Needs `layr`, `cargo-nextest` and `llvm-tools`.
  Also: `doctor` on the tiny project, `init` twice in a line (cargo and pnpm folders), the project env of `run` and
  `sweep`, the gate (one gate at a time, a stale lock, the env, the exit status), and `build_scripts_write_no_profile_into_the_project`
  (a `build.rs` and a sweep listing that compiles the build script: no stray file, no scratch folder). Unit tests: `init.rs` (idempotent edits), `doctor.rs` (the rules).
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
- The line root must be the cargo project root. The path of the `layr` executable must have no spaces
  (cargo splits the runner on spaces).
- Cross compilation: the runner variable uses the host triple.
- No schedule for `--full`.
- A child that a test kills has no coverage, see "Killed child processes".
- Child processes only have coverage when the child is a bin target built in `target/retest`. A test
  that starts a binary from another folder (Studio's gate sets `STUDIO_SUPERVISOR_BINARY` to the
  plain `target/debug` build) runs code that has no instrumentation, so a change of that binary does
  not select the test.

## Not decided

- Whether the record folder should be gzip-compressed (5.8 KB per 116 tests in the prototype).
- A faster coverage export: the lcov export of each test is still one `llvm-cov` process (about
  0.4 s). A candidate: read the profiles and the coverage mapping in process.
