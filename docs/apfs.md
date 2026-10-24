# The APFS backend (macOS)

This page is part of the [Layr documentation](../README.md#documentation).

APFS uses whole-directory clones automatically. There is no mode flag.
Trees with hard links or nested layers use file-by-file clones to preserve their behavior.
See the [10,000-file measurements](../benchmarks/apfs-performance-20261015.md) for time and space costs.


The macOS build uses APFS. The Linux build continues to use btrfs with the existing store and record formats.
The store and signed record formats remain compatible with the Linux backend.

- The store can be any folder on APFS, also on the default volume, which ignores case.
  On such a volume `README` and `Readme` cannot exist together. Layr refuses them where they could arrive:
  `init --from`, `git clone` and git fetches, peer sync, remote stores, and merges.
  The error names the paths; nothing is half written. A store on a case-sensitive APFS volume accepts them.
  Path lookups compare exact names, so a rename that changes only case (`readme` to `README`) works like any rename.
  Layr does not compare Unicode normal forms. APFS also treats `é` as one code point and as `e` plus an accent
  as the same name, on both kinds of volume. Two such names in one folder give the same result as on any APFS store.
- Commands run as the current user by default. Set `LAYR_SOCKET` to use the root service.
- The service requires a volume with ownership enabled and a separate root-owned store.
  It refuses volumes mounted with `noowners` and store folders that other users can change.
- A snapshot clones each file and checks source metadata before and after the copy.
  It is not an atomic volume snapshot. A copy of a folder that changes during the copy copies the changed
  entries again. After 6 rounds it keeps the last copy with a warning: each file is one complete version from that time.
- APFS work grows with the number of files. Files with the same valid APFS clone identity need no content comparison.
  Other files require a content comparison to find changed blocks.
  Changed ranges of large clones skip blocks at the same physical address.
- Scans read each folder with one `getattrlistbulk` call.
- `status` checks kernel change counters before and after a live read. It uses a temporary snapshot if validation fails.
  Files with an active memory mapping or an unavailable counter also use the snapshot path.
- `status` saves its result with the stat data of each entry, as git's index does. The next `status` compares only
  entries whose metadata, change counter, or clone ID differ. `LAYR_NO_STATUS_CACHE=1` compares all.
  See [APFS features](apfs-features.md).
- States are thin layers. A state in `deltas/<id>` keeps only the entries that differ from its base state
  and the paths that it removes. A chain of layers ends at a complete tree in `states/<id>`.
  See [thin APFS states](apfs-thin-states.md) and [APFS compared with btrfs and git worktrees](apfs-vs-btrfs.md).
- The index is a thin layer over the head. `add` clones only the selected entries into it.
  `add` validates the working tree before and after it copies them. Active `mmap` files use the snapshot path.
  `rm`, `mv`, `apply --cached` and `restore --staged` first make the index a complete tree.
- A regular `save -m <message>` moves the index into `deltas/<id>` and makes it immutable. Its cost depends on the changed files.
  A journal restores the index after a process stops before the database transaction completes.
  Named saves with overlays retain the existing path.
- An automatic state copies only the entries that changed since the line head. An unchanged folder makes no state.
- `status`, `add`, `save -m <message>`, `diff` and the git export read the layers directly.
  Other commands that need a folder make a view in `states/<id>`.
  `layr gc` removes views that no command used for 10 minutes (setting `thin.view_seconds`).
  It also gives a line head with more than 32 layers a complete tree (setting `thin.chain`).
- A state that is the base of another state stays on disk until that state goes, also after `gc` deletes its record.
- File modes, symbolic links, hard links and file identity survive snapshots. Nested layers stay outside history.
  The file identity for move detection is the creation time, which APFS clones, writes and renames keep.
  A move needs an identity that is unique in both trees and is not a whole second.
- Immutable flags prevent ordinary writes to states. The store owner can remove these flags.
  Use the root service for separate users and root-owned state storage.
- Overlays and peer sync are available.
- APFS sends full btrfs version 1 streams. Existing Linux peers can receive them without an update.
- APFS receives full and incremental streams. It copies clone ranges when APFS cannot share those ranges.
- APFS does not request compressed version 2 streams. Transfers between Linux peers retain their existing optimizations.
- A local restore gives the current user the admin role on the restored project.
- Transfers preserve file modes, hard links, symbolic links and extended attributes.
  macOS attribute names use a reserved `user.layr.apfs.*` namespace on Linux.
  Attributes of symbolic links use `trusted.layr.apfs.*`, because Linux forbids `user.*` attributes on symbolic links.
- Native access control lists cannot use the same rules on both systems.
  A transfer refuses macOS access control lists and Linux security attributes instead of silently losing their effects.
- Device nodes, sockets and named pipes are not supported by the APFS backend.
- APFS copies directory metadata, including access control lists and extended attributes.

Use a separate test store, for example in your home folder:

```sh
cargo build --release
export LAYR_ROOT=~/layr-test-store
./target/release/layr init app --from ~/src/app
./target/release/layr --project app --line main line new topic
```

The core command tests also run on macOS. They require GNU coreutils and GNU sed in `PATH`.
`LAYR_TEST_DIRECT_MAIN=1` removes the default `main` policy in test projects.
The Linux tests use root and already bypass that policy. This option does not test access control.

```sh
export LAYR_APFS_TEST_ROOT=~/layr-test-store
LAYR_TEST_DIRECT_MAIN=1 tests/run.sh t01-basic t02-merge t03-git t08-gitcompat
cargo test -- --ignored
```

The benchmark creates a new test directory and keeps its data and `results.json` for inspection:

```sh
python3 benchmarks/backends.py --binary target/release/layr \
  --root ~/layr-bench-1000 --files 1000 --repeats 3
```

