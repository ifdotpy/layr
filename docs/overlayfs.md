# OverlayFS backend for a Linux VM

This backend is experimental. It uses OverlayFS mounts on a local filesystem.
It requires root and permission to create mounts. It does not support a container without mount permission.

## Build and start

Build the Linux binary with the backend feature:

```sh
cargo build --release --locked --features overlayfs
```

Start the service with a new store on ext4 or XFS:

```sh
sudo env LAYR_ROOT=/var/lib/layr-overlayfs ./target/release/layr daemon
```

Use the same CLI commands for projects, lines, commits, overlays, caches and sync.
The default Linux build still uses btrfs. The macOS build still uses APFS.

## Store and snapshots

The private `.layr-overlayfs` directory contains the mount registry and file data.
The service restores the registered mounts when it opens the store after a VM restart.

A writable line has an immutable lower directory and a private upper directory.
A read-only state uses a read-only bind mount.
A new line can share the lower directory of its source state.

A snapshot of a writable line copies its visible files into a new private directory.
XFS reflinks can share the file data. On ext4, the backend copies the data and preserves sparse holes.
The backend flushes the new tree before it publishes the snapshot in the registry.
Concurrent source changes cause the snapshot to fail when the two metadata scans differ.
This check does not provide an atomic filesystem snapshot of concurrent writes.

If a source has hard links, the backend copies its tree into the new line's upper directory.
This preserves the relationship between the hard links when a process writes through either name.

## Transfer

Use backup and restore, or sync, to transfer projects between backends.
Do not open a btrfs store with the OverlayFS binary.
Do not remove the private directory or edit its registry.

The backend sends full btrfs version 1 streams through the portable stream implementation.
It preserves state identifiers and signed records. It does not send compressed extents or incremental btrfs streams.

## Limits

See the [release measurements](../benchmarks/overlayfs-20261016/README.md) for 1,000 and 10,000 files.

- Each line, state and cache requires a mount.
- Snapshots of writable trees cost time in proportion to the file count.
- ext4 copies require more space than reflinks for files with allocated data.
- A mounted cache root cannot be removed with `rm -rf`. A build can remove files inside that cache.
- Store paths cannot contain whitespace, commas, colons or backslashes.
- The portable stream rejects unsupported extended attributes and access control lists.
- The current backend has no automatic limit on the number of mounts.

## Tests

Use an isolated Linux VM with mount permission. Set the test root to a local ext4 or XFS filesystem.

```sh
export LAYR_OVERLAYFS_TEST_ROOT=/testfs
export LAYR=$PWD/target/debug/layr
bash tests/overlayfs-local.sh
bash tests/overlayfs-caches.sh
bash tests/run.sh t01-basic t02-merge t03-git t05-transfer t06-sync t07-service t08-gitcompat t11-security t13-access t16-overlays
```

Some existing btrfs tests directly create or inspect btrfs subvolumes. Those fixtures do not test an OverlayFS store.
The backend unit tests cover hard links, sparse files, cache conversion and nested mount paths.
