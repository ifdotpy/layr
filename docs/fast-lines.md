# Fast lines

A fast line is a line whose folder is a LayrFS mount: the files of its start state are read on
demand, and only the changes are written. A new line is ready in about 0.2 s for any size of
tree, and its first `status` does not read every file. Every command works on it as on a classic
line, with the same output (tests/t262-fast-lines.sh compares them).

```sh
layr line new topic --fast          # a fast line
layr line new topic --classic       # a classic line (the default)
layr config line.kind fast          # the default of the project
```

With `line.kind fast`, a line that cannot be fast (see Requirements) is made classic, with a note.
`--fast` fails with the reason instead.

## Requirements

- States with objects: `layr migrate` once per project. An overlay project has no fast lines.
- The line belongs to the user who makes it (no `--owner`).
- Linux: the service (`layr daemon`, root) and `/dev/fuse`. Every Linux build has LayrFS (FUSE).
- macOS 27 or later on Apple silicon, and the LayrFS file system extension:
  1. Install `layr-<version>.pkg`. It puts `layr` in `/usr/local/bin` and `LayrFSHost.app` in
     `/Applications`, then opens the app. Without the package: drag `LayrFSHost.app` to
     `/Applications` in Finder and open it.
  2. The app opens System Settings > General > Login Items & Extensions > File System Extensions.
  3. Turn on LayrFS.

  Only a person can turn the extension on. `layr line new --fast` tells you when it is off. Do
  not delete or move the app later: the system then turns the extension off and every fast line
  loses its folder until it is on again. Copy the app with Finder, not with `cp` or `ditto`: a
  downloaded app that a command copied runs from a temporary place, and the system does not
  register its extension.

## Measured (50,000 files, 195 MB)

| Step | Fast, macOS | Classic, macOS |
|---|---:|---:|
| `line new` | 0.15 to 0.25 s | 9 s |
| first `status` | 2.5 to 3.8 s | 17 s |
| `status` after an edit | 0.4 to 0.6 s | 5.8 s |
| `save` of one changed file | 1.0 to 1.2 s | 18 to 20 s |

The machine was loaded (load average 15 to 55). Linux numbers: [layrfs-lab.md](layrfs-lab.md).

## Caches

A cache folder (`target`, the setting `regenerable`) cannot live in the mount. It lives in
`lines/.caches/<line id>/<folder>`, and the line shows it with a link at the folder's path. Tools
see a normal folder. `save --overlays`, `line new` from a state with caches, and merges of caches
work as on a classic line. A tool that removes the folder (`cargo clean`, `rm -rf target`) removes
the link; the next save drops the cache tree, and a folder that the tool makes again becomes a new
cache. A moved link keeps its tree.

## Restarts and crashes

- Linux: the service is the file server of every fast line. A new version of the service takes
  the lines without an unmount (`layr daemon` while the old one runs): programs in the lines see
  no error. After a crash of the service or a restart of the machine, the service mounts every
  fast line again at its start (in about 0.2 s), and a command that reads a line mounts it too.
  Unsaved changes stay: they are in the line's own folder, not in the mount.
- macOS: after a restart, the user service (the LaunchAgent) mounts the user's fast lines at
  login, and any layr command that reads a line mounts it too.
- A program that had its current folder or an open file in a mount that went away (a crash, not
  an upgrade) gets errors from it: start the program again, or `cd` into the folder again.

## Limits

- No nested mounts or btrfs subvolumes and no APFS layers inside the folder (caches use links).
- `layr line show --none` (no folder) is for virtual lines only.
- macOS: FSKit has no `clonefile` inside the mount (tools/layrfs/README.md, Known limits).
- macOS: FSKit does not mount on a disk image volume ("Unable to invoke task"), so a store on
  such a volume has no fast lines. The system volume works. Other external volumes: not tested.
- Linux: the objects of a fast line's states are written whole when it saves (no deltas), so its
  history takes more space than the history of a classic line.
