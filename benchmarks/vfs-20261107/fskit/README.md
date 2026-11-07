# LayrFS: FSKit passthrough prototype

LayrFS mounts a directory at a mount point and forwards every operation to the directory with POSIX calls.
Its purpose is to measure FSKit cost for a future "virtual agent branch" in layr.
It uses the macOS 27 (FSKit V3) handler protocols and records every operation.

Requirements: macOS 27.0 or later, Xcode 27.2, a paid Apple Developer team.

## Layout

| Path | Content |
| --- | --- |
| `project.yml` | XcodeGen spec. `DEVELOPMENT_TEAM` is the only signing value. |
| `LayrFSHost/` | Host app (`dev.layr.fshost`). It only carries the extension and opens the settings pane. |
| `LayrFS/` | FSKit extension (`dev.layr.fshost.fs`, short name `layrfs`). |
| `LayrFS/Posix.swift` | Backend: `getattrlistat`, `getattrlistbulk`, `openat`, `pread`/`pwrite`, `renameat`, and so on. |
| `LayrFS/LayrItem.swift` | `FSItem` subclass and the inode table. |
| `LayrFS/LayrVolume.swift` | `FSVolume` with the V3 handlers. |
| `LayrFS/Stats.swift` | Counters, latency histogram, open log, JSON dump, and config. |
| `selftest/` | Runs the backend and the item table against a temp tree. It needs no mount and no signing. |
| `tools/fsevents-watch.swift` | FSEvents probe that `bench.sh` uses. |
| `bench.sh` | Benchmark: mount, find, stat, rg, cargo build, FSEvents, unmount. |

## Build

Generate the project after each change to `project.yml`:

```sh
xcodegen generate
```

Unsigned build (compile check only, it cannot load):

```sh
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release \
  -derivedDataPath build CODE_SIGNING_ALLOWED=NO build
```

Signed build:

```sh
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release \
  -derivedDataPath build-signed DEVELOPMENT_TEAM=XXXXXXXXXX -allowProvisioningUpdates build
```

To keep the team in the project, set `DEVELOPMENT_TEAM` in `project.yml` and run `xcodegen generate`.

Notes:

- `com.apple.developer.fskit.fsmodule` is a restricted entitlement. A provisioning profile from a paid team must allow it.
- `-allowProvisioningUpdates` registers the bundle IDs `dev.layr.fshost` and `dev.layr.fshost.fs` in the account.
- If Xcode reports "PLA Update available", sign in at developer.apple.com and accept the new Program License Agreement. Then run the build again.

Verify the signature:

```sh
APP=build-signed/Build/Products/Release/LayrFSHost.app
codesign -d --entitlements - "$APP/Contents/Extensions/LayrFS.appex"
codesign --verify --deep --strict --verbose=2 "$APP"
```

## Self-test (no mount)

```sh
selftest/run.sh
```

It compares the `getattrlist` parser with `lstat(2)` and checks directory listing, descriptor leaks, and mutations.
It also checks the parent pinning and the path update after a directory rename.

## Install and enable

1. Copy the app: `ditto build-signed/Build/Products/Release/LayrFSHost.app ~/Applications/LayrFSHost.app`
2. Launch it once: `open ~/Applications/LayrFSHost.app`
3. Check the registration: `pluginkit -m -v -p com.apple.fskit.fsmodule` (look for `dev.layr.fshost.fs`).
4. Open System Settings > General > Login Items & Extensions > File System Extensions. The host app button also opens this pane.
5. Turn on LayrFS.

An app update stops the extension and so all its mounts. Unmount before you replace the app.

## Mount and unmount

Use a normal user, not root. The mount point must be an empty directory that you own.

```sh
mkdir -p ~/mnt/layrfs
mount -F -t layrfs /path/to/source ~/mnt/layrfs
# other form (Apple passthrough sample, fsck_fskit(8)):
mount -t layrfs file:///path/to/source ~/mnt/layrfs
mount | grep layrfs
umount ~/mnt/layrfs
```

If the extension cannot read the source directory, the sandbox blocked it.
The fallback is a temporary exception in `LayrFS/LayrFS.entitlements`:
`com.apple.security.temporary-exception.files.absolute-path.read-write` with an array that contains `/`.
This is allowed for Developer ID, but not for the Mac App Store.

## Configuration

The extension reads `~/Library/Containers/dev.layr.fshost.fs/Data/layrfs.conf` when it loads a resource.
Unmount and mount again after a change.

```
# off | nocache | read | writethrough | writeback (default)
datacache=writeback
# record open and close events in the open log (default true)
logopens=true
# disk (default) | memory: build an in-memory index at load and serve metadata from it
index=disk
# 1: enumerateDirectory replies at once with no entries (measures the XPC floor; directories look empty)
null_enumerate_probe=0
```

`index=memory` walks the backing tree once when the resource loads (`getattrlistbulk`, plus `readlink` for symlinks).
The stats JSON field `index_build` has the entry counts and `build_ms`.
Lookup, getattr, enumerate, readlink and statfs then make no syscall.
Reads use `pread` on a descriptor that each item keeps open (`fd_budget` limits these to half of `RLIMIT_NOFILE`).
Create, remove, rename, setattr, write and setxattr update the index from disk after the operation.
Changes made in the backing directory outside the mount are not visible in this mode.
For hard-linked files, parent ID and added time come from one of the links.

- `datacache=off`: the volume tells FSKit to skip `FSVolume.DataCacheHandler` (`isDataCacheInhibited`). FSKit then uses `FSVolume.OpenCloseHandler`, and the kernel caches data as it decides.
- The other values grant this coherency type in `DataCacheHandler.open` and `upgrade`. FSKit cannot reduce the attribute cache or the name cache: no API controls them.
- The extension also accepts task options in the form `layrfs.datacache=read`. FSKit may not pass `mount -o` values to the module; the stats JSON records the options that arrived (`load_task_options`, `activate_task_options`, `mount_task_options`).

## Statistics

The extension counts each operation: count, errors, ENOENT, total time, maximum time, bytes, and a log2 histogram in microseconds.
Each time is module-side only, from callback entry to the reply call.
XPC and kernel time are not in it. Compare with the wall-clock time from `bench.sh`.

- Dump: `kill -USR1 $(pgrep -x LayrFS)`
- Reset the counters: `kill -USR2 $(pgrep -x LayrFS)`
- FSKit also dumps on unmount, deactivate, and unload.
- Files: `~/Library/Containers/dev.layr.fshost.fs/Data/tmp/layrfs-stats-latest.json` and one file for each dump.
- Log: `log stream --predicate 'subsystem == "dev.layr.fs"' --level info`

The JSON also has these fields:

- `open_ring`: the last 8192 open, close and cache events, with path, modes, cache mode, granted coherency, and the FSContext real and effective uid and gid.
- `caller_identity_sdk`: what the 27.2 SDK offers. `FSContext` has only `realUserID`, `effectiveUserID`, `realGroupID`, and `effectiveGroupID`. It has no pid and no audit token.
- `caller_identity_runtime`: the result of a runtime check on the first callback. It lists the `FSContext` class, its properties, and its ivars. It also has `NSXPCConnection.current()`. If that connection exists, it is the FSKit daemon, not the process that made the call.

## Benchmark

```sh
./bench.sh /path/to/checkout ~/mnt/layrfs [rust-subdir]
```

It mounts the source and runs these tests:

- `find . -type f | wc -l`, `stat` of all files, and `rg -c fn .`. Each test runs native, on a fresh mount (cold), and on the same mount again (warm).
- `cargo fetch`, then `cargo build --offline`. The build runs native and through the mount, with the same `CARGO_TARGET_DIR` on the native disk. The target directory is removed before each build.
- FSEvents: one write through the mount and one write in the backing directory. It reports whether an event arrived on the mount path within 2 s.

At the end it unmounts and collects the stats JSON. The results go to `$WORK` (printed at start), with `summary.txt` and `stats-<phase>.json`.
Terminal may ask for access to app data the first time it reads the container.
By default the script creates `<source>/.metadata_never_index` for the run, to keep Spotlight off the mount. It removes the file at the end.

## Design notes

- Items: one `LayrItem` for each backing inode. The item keeps its parent and its name, not a full path. A directory rename therefore moves all descendants.
- A live child keeps its parent item. When the kernel reclaims a directory item that still has live children, the item stays in the table until its last child goes.
- `reclaimItem` calls `FSItem.tryReclaim` under the table lock. `lookupItem` uses the same lock.
- `enumerateDirectory` reads the whole directory with `getattrlistbulk` on the first call and closes the descriptor at once. The listing stays in memory until the last entry is packed. The cookie is the entry index. The verifier comes from the directory mtime and ctime. This avoids the descriptor leak and the `telldir` problem of Apple's sample.
- Every result has all attributes. FSKit caches all populated attributes (`FSVolumeHandlerResult.h`).
- Read and write use `pread`/`pwrite` on the descriptor that the item opened in `open`. If the item has no open descriptor, the call opens a temporary one; the stats count these (`temp_opens_for_io`).
- Root: the backing root inode shows as `FSItem.Identifier.rootDirectory` (2).

## Known limits

- Changes made in the backing directory outside the mount do not reach the kernel caches. FSKit has no change-notification call. With `datacache=writeback`, writes through the mount can reach the backing files later than they would on a normal mount.
- FSKit fails `renamex_np` flags (`RENAME_SWAP`, `RENAME_EXCL`) with `ENOTSUP` (`FSVolume.h`). LayrFS implements only plain rename.
- Hard links: an item keeps the last name that a lookup used. After you remove that name, the item fails with `ESTALE` until a lookup through another link.
- Item IDs are backing inode numbers. Do not put other mount points inside the source directory.
- No `clonefile`, `ioctl`, `fsctl`, byte-range locks to the module, ACLs, preallocate, or `SEEK_HOLE`.
