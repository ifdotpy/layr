# Virtual lines (experimental, macOS and Linux)

This page is part of the [Layr documentation](../README.md#documentation).

A line is a pointer to a state plus its unsaved changes. Its folder is one way to show it.
On macOS, a virtual line shows the line as a LayrFS mount: the tree of the start state stays where it is, and only the
changed files are stored. A new virtual line costs one mount, not one clone of the whole tree.

The feature needs `LAYR_EXPERIMENTAL_VIEWS=1`. On macOS it uses macOS 27 and the LayrFS file system extension,
installed and enabled in System Settings ([tools/layrfs/README.md](../tools/layrfs/README.md)).
On Linux it uses the configured Btrfs or OverlayFS backend and the normal Layr daemon.
Linux does not need FUSE or the macOS extension.

## Use

```sh
export LAYR_EXPERIMENTAL_VIEWS=1
layr line new topic origin/main --view virtual   # mount at lines/topic
cd ../topic                                      # edit, build, run tools as in a folder
layr status                                      # the changes, from the upper folder
layr diff
layr save -m "message"                           # every change of the view becomes the new head
layr line show topic --none                      # unmount; the changes stay
layr line show topic --virtual                   # mount again
layr line show topic --folder [<path>]           # a native folder with the same content
layr line delete -D topic                        # unmount; unsaved work goes into the final state
```

Run `line show` and `line delete` outside the line's folder.

## Linux backend

Linux uses native copy-on-write trees: a Btrfs snapshot or an OverlayFS branch.
Both share unchanged data with their source. The view commands have the same lifecycle
as macOS: create, edit, status, diff, save, hide, show, convert, and delete.
A named save includes new and changed files and deletions, without an explicit `add`.
Ignore rules exclude new build output. A hidden line can still run `status` and `save`
through `--project` and `--line`.

`line show --none` moves the working tree to `meta/vlines/<line id>/tree` and records
its machine-local path. `--virtual` moves the same tree back. These commands preserve
unsaved changes. A repeated hide or show is safe. `--folder` removes the virtual-view
restriction; it also accepts an external path on the same file system.
A private move journal allows `line show` to recover after a failure between the tree
move and the path-index update. If both possible paths exist, recovery refuses to
choose one and preserves both.

Linux uses the existing native snapshot, Git bridge, cache, and search paths.
Status and diff include all nonignored changes, including new files. Status marks them as staged.
It retains the `.git` bridge directory. This is functional parity of the view commands,
not an FSKit emulation or an in-memory file system. No equal-speed claim is made.
The limits on the native Btrfs and OverlayFS backends still apply.

Run `tests/views-parity.sh` as a non-root user with a separate `LAYR_ROOT`, `LAYR`, and
(on Linux) `LAYR_SOCKET` for that store's daemon. The same script checks FSKit on macOS,
Btrfs on Linux, and OverlayFS on Linux. It checks saved content, deletions, modes,
symlinks, ignored files, search results, hidden saves, and recovery of a failed Linux move.

## Git commands

Use `layr git remote`, `layr git fetch`, `layr git push`, and `layr git sync` from a virtual line.
They use the existing project Git store. `sync` exports the saved head locally; `push` sends it to the remote.
Save your changes first. Neither command includes unsaved files.
On macOS, these commands do not add a `.git` file or directory to the mount.
On Linux, the existing `.git` directory stays as it is.
Hidden lines can use the same commands with `--project` and `--line`.

`layr git pull` fetches remote commits and merges them into the virtual line.
Use `--ff-only` to refuse a merge, or `--rebase` (`-r`) to replay local commits.
An update refuses to overwrite unsaved changes. Conflicts appear in `status`; resolve them and save.
A hidden macOS line mounts for the command and returns to its hidden state after success or failure.
The Git command support adds no export step to virtual saves on macOS.

## macOS backend design

| View | What is on disk | Cost of a new line |
| --- | --- | --- |
| folder | an APFS clone of the whole tree | one clone of every entry |
| virtual | an upper folder with the changed files, and a mount | one mount |
| none | the line record and its upper folder | nothing |

- **Lower tree.** The complete tree of the start state, `states/<id>`. It is sealed (immutable flags) and
  never changes while the line uses it. A thin state gets a complete tree once, the first time a command needs it
  (`Repo::state_root`); later virtual lines of the same state use it at once.
  `layr gc` keeps the lower tree of every virtual line (`views::protected_states`).
- **Upper folder.** `meta/vlines/<line id>/upper`, on the same volume as the lower tree. It is a thin layer
  ([thin APFS states](apfs-thin-states.md)): changed and new entries, and the removed paths in the attribute
  `com.layr.removed` of its root. Layr reads it as `thin::Layered::over(upper, lower)`, the same reader as states.
- **Mount.** `mount -F -t layrfs -o nobrowse meta/vlines/<line id> lines/<name>`. The file
  `meta/vlines/<line id>/layrfs-view.conf` names the lower tree. LayrFS merges the upper folder over the lower
  tree and copies a lower file up with `clonefile` at its first change. The lower tree is read on demand, one
  `getattrlistbulk` call per folder, and kept in the extension's memory.
- **Local record.** `meta/vlines/<line id>/view.json` holds the view kind, the base state and the mount state.
  It is local to this machine and never part of a record.
- **Status and diff.** The candidates are the entries of the upper folder, its removed paths, and the paths that
  earlier saves of the line changed (the layers of the head above the lower tree). Each candidate is compared with the
  head. Nothing scans the merged tree. Ignored new files (`.gitignore`) are left out, as `status` does.
- **Save.** `layr save -m` writes a thin layer over the head with the changed entries, cloned from the upper folder,
  and the removed paths. The line keeps its mount and its lower tree: after the save the upper folder equals the
  new head, so `status` is clean. All changes are saved: a virtual line has no index.
  `layr save` without a message records an automatic state and keeps the named head. An unchanged
  folder reuses its previous automatic state. If hard links require a complete state, save writes
  that state instead of a thin layer.
- **Delete.** Unsaved changes become the final automatic state, a thin layer over the head. Then the mount, the
  line folder and the upper folder go.
- **Folder.** `line show --folder` clones the lower tree, applies the upper folder (`Layered::materialize`),
  prepares the folder as `line new` does, and replaces the mount with it.

## macOS backend limits

- Only these commands run on a virtual line: `status`, `diff` (`-R` is not available), `save`, `save -m` (`-F` and `-q`
  too, no `--amend`), `log`, `show`, `rev-parse`, `merge-base`, `line new/list/delete/show`, `gc`, `config`, `git remote/fetch/pull/push/sync/clone`.
  `layr du` leaves virtual lines out (their cost is the upper folder).
  The others (`add`, `restore`, `reset`, `merge`, `undo`, and so on) refuse with a message that names
  `line show <line> --folder`.
- No `.git` folder and no cache overlays. Build caches (`target/`, `node_modules`) are written into the upper
  folder; ignore rules keep them out of status and saves.
- A rename of a folder that the start state has fails with `EXDEV` in the mount. `mv` then copies and removes;
  a program that renames a large folder pays for the copy.
- No `clonefile` inside the mount (an FSKit limit). `cp -c` fails; `cp` copies.
- Hard links: a save of a hard-linked new file fails; use `line show --folder`.
- The upper folder grows with every file that the line ever changed. Saves do not empty it; status compares each of
  its files with the head.
- `layr gc` gives a head with more than `thin.chain` layers a complete tree. Then the head no longer rests on the
  lower tree, and `status` compares the whole trees.
- The extension's sandbox puts `com.apple.quarantine` (agent `LayrFS`) on each file it makes, and cannot remove it.
  The mount hides the mark, and Layr removes it from the upper folder before a save or a conversion.
- The extension serves every call. Directory listings cross XPC every time, so `find` over a large tree is slower
  than in a folder. Small writes cost about 2.5 ms each. Agents search with `layr tool rg|find` (docs/layrfs.md):
  the tool server reads the lower tree and the upper folder directly and never goes through the mount.
- A crash of the extension ends the mount. The upper folder and the changes in it stay; `line show --virtual`
  mounts it again.

## Measurements

Chromium `src` (515,999 files), median of 3 runs, seconds. The Mac was under other load (load average 30 to 75).
Details: [benchmarks/views-20261107](../benchmarks/views-20261107/README.md).

| Step | Folder line | Virtual line |
| --- | ---: | ---: |
| `line new` | 65.8 | 0.29 |
| `status` after 10 edits | 18.1 | 0.34 |
| `save` of the 10 edits | 35.5 | 0.38 |
| `line delete` | 81.5 | 0.39 |
| `rg -l TODO` (warm) | 21.7 | 254.8 |
| `find . -type f` (warm) | 7.3 | 17.3 |

The states that both lines saved are equal. Line operations no longer depend on the size of the tree.
Bulk reads of file content through the mount are slow: `rg` over the whole tree took 12 times as long.

## Views of overlays

A view is a line (or a state) with an ordered list of overlays (overlays) above it
([overlays](overlays.md) section 1). The working folder of a line is its default view. Since slice 5
(overlays.md section 1a) the view, not the line, holds the overlays:

- A new line folder shows every overlay that its owner reads, in the project order of overlays
  (`layr overlay order`). An overlay with a `platform` that is not this machine's is left out.
- `layr overlay use <names>` changes the view of the current working folder only. The line keeps
  its parts on the other overlays: a save does not clear them.
- `layr view <line or state> --overlay a,b` prints another view of the same line or state, with
  those overlays. `--folder <dir>` writes its files to a new folder (on the volume of the store).
  That folder is a copy to read or build in, not a working folder: it has no status and no save.

## Open items

- Read path: a read of a file whose descriptor is not cached opens it again (one `openat` per read), and the
  extension used less than one core during `rg`. The kernel data cache (`datacache=writeback`) did not help
  (253 s cold, 435 s warm). Candidates: a descriptor cache with eviction, fewer calls per read, and FSKit kernel
  offloaded I/O.
- A thin lower tree (the layers of a state) instead of one complete tree, so that a virtual line of a thin state
  costs no clone.
- Cache overlays as native folders, linked from the view. Owner decision 2026-11-11: a cache is an ordinary
  overlay in v2 objects, and a view joins a line and a set of overlays (docs/overlays.md section 1), so the
  view shows a cache overlay as it shows any overlay.
- More commands: `restore`, `reset`, `merge`.
- Replace the lower tree under a mounted line. Git pull and replay currently apply changes to the upper folder.
