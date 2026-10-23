# The APFS backend (macOS)

This page is part of the [Layr documentation](../README.md#documentation).

APFS uses whole-directory clones automatically. There is no mode flag.
Trees with hard links or nested layers use file-by-file clones to preserve their behavior.
See the [10,000-file measurements](../benchmarks/apfs-performance-20261015.md) for time and space costs.


The macOS build uses APFS. The Linux build continues to use btrfs with the existing store and record formats.
The store and signed record formats remain compatible with the Linux backend.

- The store requires a case-sensitive APFS volume.
- Commands run as the current user by default. Set `LAYR_SOCKET` to use the root service.
- The service requires a volume with ownership enabled and a separate root-owned store.
  It refuses volumes mounted with `noowners` and store folders that other users can change.
- A snapshot clones each file and checks source metadata before and after the copy.
  It fails if the source changes during the copy. It is not an atomic volume snapshot.
- APFS work grows with the number of files. Files with the same valid APFS clone identity need no content comparison.
  Other files require a content comparison to find changed blocks.
- `status` checks kernel change counters before and after a live read. It uses a temporary snapshot if validation fails.
  Files with an active memory mapping or an unavailable counter also use the snapshot path.
- `add` clones changed files into the private index instead of copying all bytes. Hard-linked index files use the existing copy path.
- `add` validates the working tree before and after it copies selected entries. It avoids full temporary snapshots when validation succeeds.
  The first `add` still creates a complete index from the head. Active `mmap` files use the snapshot path.
- A regular `commit` moves the prepared index into the state store and makes it immutable.
  A journal restores the index after a process stops before the database transaction completes.
  Commits with overlays retain the existing path.
- File modes, symbolic links, hard links and file identity survive snapshots. Nested layers stay outside history.
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

Use a separate test store on an existing case-sensitive APFS volume:

```sh
cargo build --release
export LAYR_ROOT=/Volumes/YourCaseSensitiveVolume/layr-test-store
./target/release/layr init app --from ~/src/app
./target/release/layr --project app --line main branch topic
```

The core command tests also run on macOS. They require GNU coreutils and GNU sed in `PATH`.
`LAYR_TEST_DIRECT_MAIN=1` removes the default `main` policy in test projects.
The Linux tests use root and already bypass that policy. This option does not test access control.

```sh
export LAYR_APFS_TEST_ROOT=/Volumes/YourCaseSensitiveVolume
LAYR_TEST_DIRECT_MAIN=1 tests/run.sh t01-basic t02-merge t03-git t08-gitcompat
cargo test -- --ignored
```

The benchmark creates a new test directory and keeps its data and `results.json` for inspection:

```sh
python3 benchmarks/backends.py --binary target/release/layr \
  --root /Volumes/YourCaseSensitiveVolume/layr-bench-1000 --files 1000 --repeats 3
```

