# Virtual lines (experimental, macOS)

This page is part of the [Layr documentation](../README.md#documentation).

A line is a pointer to a state plus its unsaved changes. Its folder is one way to show it.
A virtual line shows the line as a LayrFS mount: the tree of the start state stays where it is, and only the
changed files are stored. A new virtual line costs one mount, not one clone of the whole tree.

The feature is an experiment. It needs `LAYR_EXPERIMENTAL_VIEWS=1`, macOS 27, and the LayrFS file system extension,
installed and turned on in System Settings ([tools/layrfs/README.md](../tools/layrfs/README.md)).

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

## Design

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
- **Delete.** Unsaved changes become the final automatic state, a thin layer over the head. Then the mount, the
  line folder and the upper folder go.
- **Folder.** `line show --folder` clones the lower tree, applies the upper folder (`Layered::materialize`),
  prepares the folder as `line new` does, and replaces the mount with it.

## Limits

- Only these commands run on a virtual line: `status`, `diff` (`-R` is not available), `save -m` (`-F` and `-q`
  too, no `--amend`), `log`, `show`, `rev-parse`, `merge-base`, `line new/list/delete/show`, `gc`, `config`.
  `layr du` leaves virtual lines out (their cost is the upper folder).
  The others (`add`, `restore`, `reset`, `merge`, `git push`, `undo`, and so on) refuse with a message that names
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
  than in a folder. Small writes cost about 2.5 ms each.
- A crash of the extension ends the mount. The upper folder and the changes in it stay; `line show --virtual`
  mounts it again.

## Measurements

See [benchmarks/views-20261107](../benchmarks/views-20261107/README.md).
