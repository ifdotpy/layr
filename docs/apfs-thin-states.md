# Thin APFS states

## Problem

A btrfs snapshot shares the metadata of the source tree. It copies metadata only when entries change.
APFS has no writable snapshot of a folder for a normal user. A directory clone shares file data,
but it writes a new inode, directory record and attributes for every entry.

On a test volume, one clone of 10,000 entries costs about 280 bytes per entry for empty files
and about 330 bytes per entry for 4 KiB files.
Before this change, each line, index, named state and automatic state of a 10,000-file project
added about 3.2 to 3.9 MiB on APFS. On btrfs, each added less than 0.3 MiB.

## Format

- `states/<id>`: a complete tree. It is a keyframe, or a view of a thin state.
- `deltas/<id>`: a thin layer. It holds only the entries that differ from the base state.
- The marker attribute `com.layr.subvolume` of a layer root names its base state.
- The attribute `com.layr.removed` of a layer root lists the removed paths.
- A chain of layers ends at a complete tree.

Layer rules:

1. An entry in a layer replaces the same path in the layers below it.
2. A folder in a layer merges with the folder below it, unless the layer also removes that path.
3. A removed path hides the path and all of its content in the layers below.
4. Every folder in a layer has the metadata of that folder in the complete tree.
5. The ancestors of every written or removed path are folders in the layer.

An entry is written to a layer when its type, mode, owner, size, modification time,
link target or APFS clone identity differs. Equal clone identities with valid generation
counts prove equal data. Folder sizes are not compared, because they count one layer only.

## Bases

| New state | Base |
| --- | --- |
| Named save from the index | The line head |
| Named save without staged changes (`--allow-empty`, `--amend`) | The line head |
| Automatic state | The line head |
| Index | The line head |

Automatic states never use other automatic states as a base. Retention can delete them freely.
Merges, imports, received states and lines with overlays keep complete trees.

A state that is the base of another state stays on disk until that state goes.
This is also true when `gc` deletes its record. A later `gc` deletes it.
`overlay move --purge` cannot wait: it gives the upper states complete trees, then deletes the state.
`gc` also deletes a layer that has no record after one hour: a process stopped before it wrote the record.

## Readers

`fsutil::Src` is a tree to read: a folder or a `thin::Layered`.
These paths read layers without a complete copy:

- `status`, `add`, `save -m <message>`, `diff`, `restore`, `park` and `clean`.
- Change lists (`changes::file_changes`), diffs, rename detection and ignore rules.
- The git export. Git reads only the changed paths, from a small private copy.

Other commands call `Repo::state_root`. It makes a view in `states/<id>`.
`layr gc` removes views that no command used for `thin.view_seconds` (default 600).
It gives a line head with more than `thin.chain` layers (default 32) a complete tree.

`rm`, `mv`, `apply --cached` and `restore --staged` make the index a complete tree first.
Their writes do not support thin layers.

## Hard links

A recursive clone copies hard links as separate files. Thin layers clone each entry alone.

- A complete tree with hard links is copied file by file. Its links survive.
- A layer refuses to write a file that has more than one name. The caller makes a complete tree.
- `add` of a file with more than one name uses a complete index.

## File identity

Move detection needs an identity that survives a clone. APFS clones, writes and renames keep the creation time.
Layr uses the creation time as the identity. It no longer writes `com.layr.origin` on each entry.
That attribute cost about 80 bytes per entry in each tree.

A tool that sets an old modification time (tar, zip, npm) also lowers the creation time to it.
Many unrelated files then share one creation time, usually with zero nanoseconds. So:

- A creation time with zero nanoseconds is no identity for moves.
- A deleted file and a new file are a move only when their identity is unique in both trees.

## Limits

- Each line still has one complete working folder. A new line costs one tree clone.
- A command that needs a view of a thin state costs one clone of the keyframe until `gc` removes the view.
- Long chains make reads slower until `gc` makes a keyframe.
- Sends to peers use complete trees. The receiver stores complete trees.
