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
A read-only state uses a read-only mount of its saved layers.
A new line can share the lower directory of its source state.

A snapshot copies the upper layer into a new private directory. It shares the unchanged lower layers.
The live mount does not change. Open files and current directories continue to refer to the same working tree.
Later writes cannot change the saved copy.

The upper layer contains all changes since the line's base. Later snapshots can copy those changed files again.
The backend does not rotate the live upper layer after each commit.
An ownership change can also copy a lower file into the upper layer. A service branch with a different owner can therefore copy many files.
XFS reflinks can share copied data. On ext4, the backend copies allocated data and preserves sparse holes.
The backend flushes new data before it publishes the snapshot in the registry.
Concurrent source changes cause the snapshot to fail when the two metadata scans differ.
This check does not provide an atomic filesystem snapshot of concurrent writes.

The backend combines the layers before a new snapshot would reach 17 layers or exceed its mount-option budget.
This operation copies the visible tree once. A moved lower directory also requires a copy of that directory.
Trees with hard links use a full copy. New lines copy these trees into their upper directory.
This preserves the relationship between hard links when a process writes through either name.

## Caches

A working cache is an ordinary directory. Tools can remove it with `rm -rf` and create it again.
The registry keeps the cache path. Snapshots exclude the contents at that path, including after directory replacement.
Saved caches use separate copies. A restored cache has its own files and cannot change another line's cache.
On ext4, these copies require additional space. XFS can use reflinks.

The backend converts old cache mounts when it opens the store.
It first makes the old cache read-only, saves a durable copy, and records the conversion before it removes the mount.
Close open files if the conversion reports a busy mount. The next open resumes an incomplete conversion.
Do not use an older OverlayFS binary after this conversion. Use backup and restore to transfer to another backend.

## Transfer

Use backup and restore, or sync, to transfer projects between backends.
Do not open a btrfs store with the OverlayFS binary.
Do not remove the private directory or edit its registry.

The backend sends full btrfs version 1 streams through the portable stream implementation.
It preserves state identifiers and signed records. It does not send compressed extents or incremental btrfs streams.

## Limits

See the [release measurements](../benchmarks/overlayfs-20261016/README.md) for 1,000 and 10,000 files.

- Each line and state requires a mount. Working caches do not.
- Snapshots still scan file metadata. A large upper layer increases the copy cost.
- ext4 copies require more space than reflinks for files with allocated data.
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
The backend unit tests cover hard links, sparse files, cache conversion, open writers, layer compaction and mount paths.
