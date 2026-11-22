# Git history in the object store

The files of a project's git history live once, in the object store (`meta/objects`, store v2).
The git store (`<project>/git`) keeps commits, trees and tags, and the few blobs whose git bytes
are not the bytes of any file of a state (a line end filter changed them, for example).

Before, both stores held every file. On 1,000 commits of Chromium that was about 2.7 GB; now it
is about 1.40 GB with the object store's blocks of 256 KiB (1.27 GB with blocks of 4 MiB, but git
reads them 2 to 3 times slower; `benchmarks/git-on-v2-20261119/README.md`).

## How git reads the files

- **The map** (`meta/gitpack/segments/*.map`) names the git blob id of each file content of the
  object store: the id, the size, the CRC-32 of its pack entry, and the object store blob (or
  the target of a symbolic link). Each `layr gc` adds the contents of new states as one new
  segment: newest state first, and in path order inside a state. Trees that one run walked are
  not walked again (`meta/gitpack/trees`).
- **The folder** `<project>/git/layr-v2` shows each segment as one git pack,
  `pack/pack-<id>.pack` with its `.idx`.
  - Linux: a FUSE mount of `layr fs gitpack serve` (the cargo feature `layrfs-fuse`).
  - macOS: a mount of the LayrFS extension (FSKit) in its git pack mode: the folder
    `meta/gitpack/fsk` has a `layrfs-gitpack.conf` file. The extension reads the bytes from
    `layr fs gitpack serve`. Its sandbox refuses Unix sockets, so each connection is a pair of
    named pipes in `meta/gitpack/pipes` (`tools/layrfs/LayrFS/GitPackVolume.swift`).

  The index is made when the folder mounts. A pack entry is made when git reads its bytes: the
  file from the object store, as zlib "stored" blocks (not compressed). The pack holds the entries
  in the order of the map, so the bytes after a read are usually the files that git reads next.
- **The git store** names the folder in `objects/info/alternates`. The `.git` folder of each line
  names the git store there, so git in every line reads the files too.

The service mounts the folder when it starts. A layr command that opens the git store mounts it
when it does not answer.

## What `layr gc` does

1. Imported states that have no objects get them: from the objects of the first parent and the
   changed paths when that parent is an imported state with objects, else from the whole tree of
   the commit. A `state.tree` record names them.
2. The map gets the contents of new states.
3. The folder mounts, and the git store names it.
4. The git store writes one new pack of every object that the map does not serve, then removes
   its old packs and loose objects. The fast lines replace their clones of the old packs with
   clones of the new one.

`layr init --from-git` runs `layr gc` after `layr migrate`. A prune of the object store keeps
every blob that the map names: the git store may hold no other copy.

## Push

Entries of the folder are not compressed, and git sends an entry that it reuses as it is. Before
`layr git push`, the git store makes a compressed pack of the objects to push (git prefers a local
pack). A store push of git packs (`docs/remote-format.md`, "Git packs") makes its entries again.

## Limits

- Linux builds without LayrFS (`layrfs-fuse`) do not mount the folder: there the git store keeps
  every blob, as before. On macOS the LayrFS extension must be on.
- `git.objects = "git"` in the project config keeps every blob in the git store.
- A git command in a line that needs a file reads it through the folder. When the folder does
  not answer, the command fails until a layr command or the service mounts it again.

Run `tests/git-on-objects.sh` (Linux with a private root daemon, or macOS with the LayrFS
extension). It checks a history with changed, added and removed files, an executable, a link and a
large file: after `layr gc` the git store holds no blob, `git log -p` gives the same output, a new
commit of git in a fast line moves too, a push gives a complete and compressed remote, and the
project deletes.
