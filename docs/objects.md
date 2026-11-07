# Content-addressed objects (phase 1, experimental)

Status: experimental. The crate `crates/layr-objects` and the command `layr objects` write and
read the objects described here. No other command reads them yet: states, lines, merges and
remotes still use file system snapshots. This file is the format contract of the slice and the
plan for the next steps.

## 1. Goal

Today a state is a snapshot of a folder (a btrfs subvolume, an APFS clone or thin layer, or an
overlayfs layer). Phase 1 moves the truth to a content-addressed object store:

- A **blob** is the bytes of one file.
- A **tree** is one folder: its entries with full POSIX metadata, and the ids of their blobs and
  trees. Trees form a Merkle tree, so the id of the root tree names the complete content.
- A state record names the id of its root tree. The identity of a state is the hash of its record.
  Checks and attestations name the tree id, so evidence for a tree holds for every state with
  that tree.

The stored form of a blob is free: whole (so a checkout clones it), one zstd frame, a delta
against the previous version, or content-defined chunks. The id never depends on the form.

## 2. Object ids

```text
id = SHA-256("<kind> <decimal length>\0" || content)        Public and Private projects
id = HMAC-SHA256(key, "<kind> <decimal length>\0" || content)  Secret projects
kind = blob | tree | chunk
```

- **SHA-256** is the hash of persisted content in layr (`layr_hashers::ContentAddress`; the store
  remote and the chunk names use it). It runs with the ARMv8 SHA extensions and SHA-NI, so hashing
  is not the bottleneck: a second import of Chromium (4.5 GB of files) hashes everything in 33 s.
  BLAKE3 is faster on one core but is reserved for disposable local checks
  (`crates/layr-hashers`), and a format hash must never change.
- The typed header makes a blob and a tree with equal bytes different objects. It is the header of
  Git's SHA-256 object format, so **a layr blob id equals the Git SHA-256 blob id** of the same
  bytes (the id of the empty blob is `473a0f4c...721813` in both). Tree ids differ from Git: a
  layr tree keeps metadata that Git does not.
- The length in the header is checked: a file that changes while it is read gives an error, not a
  wrong id.

### Secret projects

In a Secret project the ids are keyed, so equal content gets equal ids only inside the project,
and a store (or a server) cannot test whether a project holds a known file. The key is
`HKDF-SHA256(ikm = project identity, salt = project id, info = "layr object id key v1")`
(`layr_objects::id::project_key`), the same construction as the cache chunk keys
(`layr_store::chunks::key`). Public and Private projects use plain SHA-256, so equal files dedup
across projects and match Git.

`config.json` of a store records the id function; a store opened with another one is refused.
In this slice `layr objects` uses plain ids for every project: the library supports keyed ids, and
the command will pass the key when the project key is readable at this point (section 9).

## 3. Tree encoding

A tree is a canonical byte string. Numbers are unsigned LEB128 varints in their shortest form.

```text
tree    = version:u8 (1)  count:varint  entry*  groups:varint  group*
entry   = name:bytes  type:u8  mode:varint  body  xattrs:varint  (name:bytes value:bytes)*
body    = file:      size:varint  blob-id:32
          folder:    tree-id:32
          symlink:   target:bytes
group   = paths:varint  path:bytes*
bytes   = length:varint  raw bytes
type    = 1 file, 2 folder, 3 symbolic link
```

- Entries are sorted by name bytes, names are unique. A name is any bytes except `/` and NUL, not
  empty, not `.` or `..`, so non-UTF-8 names are kept as they are.
- `mode` is `st_mode & 0o7777`: permission bits with setuid, setgid and sticky. A symbolic link
  has mode 0 (Linux ignores it, and macOS gives it the umask).
- An empty folder is a folder entry that names the empty tree (`01 00 00`).
- Extended attributes are sorted by name and kept with their exact bytes. Left out:
  `com.apple.provenance` (the system stamps it on files), layr's own markers (`com.layr.`), and
  the classes `trusted.`, `security.` and `system.` (overlayfs internals, SELinux labels, POSIX ACLs:
  they need privileges or belong to the machine).
- **Hard links:** the root tree of a write lists groups. Each group holds the paths (relative to
  the root, sorted) of one inode that has more than one name in the folder. Only the root has
  groups, so every tree below it is pure content and is shared between states.
- Not kept: owners and times. A checkout gives every entry to the caller (layr sets line
  owners), and times are the time of the checkout. Including them would give two equal folders
  different ids and stop dedup between lines.
- A FIFO, a socket or a device in a folder stops the write with an error.

The decoder refuses every form the encoder does not make (unsorted or duplicate names, a long
varint, a bad mode, bytes after the end) and then checks `encode(decode(x)) == x`. One folder has
one encoding and one id.

## 4. Stored forms

Every object except a hot blob is one file:

```text
"LOB1"  kind:u8 ('b' blob, 't' tree, 'c' chunk)  form:u8  body
form 'z' whole:    one zstd frame with its content size (trees, chunks, small cold blobs)
form 'd' delta:    depth:u8  base-id:32  zstd frame made with the base as a raw dictionary
form 'c' chunked:  count:varint  (chunk-id:32  length:varint)*
```

- **Hot blob:** the exact file bytes in `blobs/`, made as a clone of the source (`clonefile` on
  APFS, `FICLONE` on btrfs and XFS, a copy elsewhere). A checkout clones it back, so a folder
  view costs metadata only. The current bases of lines are hot.
- **Whole:** `layr_store::zst::compress`, zstd level 3.
- **Delta:** a new version of a path is stored against the blob at the same path in the previous
  tree, as `zstd --patch-from` makes it (`layr_store::zst::compress` with a base: the window covers
  both inputs and long-distance matching is on). Both versions must be at most 64 MiB. A chain is
  at most 16 deltas deep; the next version is stored whole (a key frame), so a read decodes at
  most 16 deltas.
- **Chunked:** a cold file of 1 MiB or more without a delta base is cut with the FastCDC chunker
  of the cache remote (`layr_store::chunks`: 16 KiB minimum, 64 KiB average, 256 KiB maximum).
  Each chunk is a `chunk` object, compressed, so equal parts of large files (build output, a
  second line) are stored once. The cut is computed as a stream: a file is never read into memory
  whole.
- **Packs** (next step): many small objects in one file, as `layr_store::chunks::Builder` packs
  cache chunks. They are needed: on APFS a loose object takes at least one 4 KiB block (section 8).

Every read checks the content against the id (a blob after the delta or chunk decode, each chunk,
each tree). A checkout clones hot blobs without a read; `layr objects verify` compares the bytes.

## 5. Layout

```text
<project>/meta/objects/
  config.json          {"ids":"sha256","v":1}
  blobs/ab/cdef...     hot blobs (file bytes)
  objects/ab/cdef...   trees, cold blobs, chunks ("LOB1" files)
  states/<state id>    the root tree id of an imported state
  tmp/                 files being written
```

The first two hex digits of an id name a fan-out folder. **Writes are idempotent and free of
garbage:** a writer checks for the object, writes a temporary file in `tmp/`, and renames it to
its final name with `RENAME_NOREPLACE` (`renameatx_np(RENAME_EXCL)` on macOS; a link and an unlink
where the file system has no such rename). An existing name means the same content is already
there, so the temporary file goes and nothing is rewritten. A failed write removes its temporary
file. Concurrent writers of the same object agree; a reader never sees a partial object.

A hot write clones the source to `tmp/`, hashes the clone, and stores it under that hash, so a
source that changes during the write cannot store wrong bytes under an id. A sealed APFS state
gives its files the immutable flag, which a clone keeps; layr clears it on the clone
(`sys::apfs_unseal_fd`, the only code with `unsafe`, stays in `src/sys.rs`).

## 6. Writing and reading a tree

- **Write** (`layr_objects::write_tree`): a parallel walk (rayon) reads each folder, `lstat`s and
  lists extended attributes of each entry, stores files and subfolders in parallel, and writes each
  tree bottom up. A file that the store holds is hashed and skipped.
- **Checkout** (`layr_objects::checkout`): makes folders, then files and links in parallel: a hot
  blob is cloned, another blob is decoded and written. Then the hard link groups, then the folder
  modes deepest first (a read-only folder must be full before it closes).
- **Compare** (`layr_objects::compare`): an independent walk of two folders that compares names,
  types, modes, bytes, link targets, kept extended attributes and hard link groups. It shares no
  code with the writer, so a writer fault shows as a difference.

## 7. Commands

```text
layr objects import <rev> [--cold] [--parent <tree>] [--json]
layr objects verify <rev> [--cold] [--json]
layr objects stat [--json]
layr objects checkout <tree or rev> <folder>
```

- `import` writes the tree of a state (read through `Repo::state_root`, as `layr export` reads
  it) and records it in `states/`. It prints the tree id first. `--cold` stores new blobs
  compressed, as deltas against the tree of the state's first parent when that state was
  imported (or against `--parent`). Without `--cold` new blobs are hot. Cache and derived states
  are refused (`Repo::source_state`).
- `verify` imports the state, checks out its tree into the project's work folder (same volume,
  so hot blobs are clones), compares every entry and byte with the state, and removes the copy.
  Exit 1 on a difference.
- `stat` counts objects and their bytes by form.
- `checkout` makes a folder from the tree of an imported state. It needs the `export` right and
  passes the secret check of `layr export`. On a root service (Linux) it runs only for root in this
  slice.

## 8. Measurements

macOS 27.2, M5 Max, APFS, release build. Store bytes are file sizes; "allocated" counts 4 KiB
blocks.

**History of this repository** (the last 60 first-parent states of main, ba25720 (#384) to e900d3f (release 0.39.0),
imported with `--cold` in order, each against its parent; a test store made with
`layr git clone`). A state is about 671 files, 8.6 MiB.

| store | bytes |
| --- | --- |
| objects, all 60 states | 3.15 MiB (6.6 MiB allocated, 1274 files) |
| of which the first tree's 671 blobs, whole zstd | 2.51 MiB |
| of which 244 later file versions as deltas | 0.10 MiB |
| of which 298 trees | 0.54 MiB |
| current backend, the same 60 states (APFS thin states and clones, `layr du --store` growth without `meta`) | 20.4 MiB |
| Git pack of the full history (909 commits, all refs) | 29.9 MiB |

Each import takes about 0.25 s; `verify --cold` of the oldest, a middle and the newest state
passes (decode, checkout and compare in under 0.7 s each). The deltas match the earlier estimate
(0.12 MiB). The first tree costs 2.5 MiB as separate frames against 1.5 MiB as one zstd-19
stream, and the loose files double the allocated size: packs (section 4) fix both.

**Chromium** (`src` at efd35c5, 516,265 files, 47,031 folders, 4.47 GB of file bytes; `.git` left
out; `objects-bench` example of the crate):

| operation | time | store |
| --- | --- | --- |
| hot import, empty store | 149 s | 480,076 new hot blobs as clones (no data blocks copied) |
| hot import again (all objects present) | 33 s | 0 new objects |
| checkout with clones | 78 s | 516,265 clones |
| compare checkout with source | 37 s | 563,317 entries equal |
| cold import, empty store | 233 s | 1.62 GB (36 % of the file bytes): 1.22 GB whole zstd blobs, 0.37 GB in 10,159 chunks of 306 files of 1 MiB or more, 26 MB of trees; 3.1 GiB allocated (536,154 loose files) |
| checkout from the cold store (decode) | 167 s | 516,265 files written |
| compare that checkout with source | 67 s | 563,317 entries equal |
| `cp -cR` of the same folder (APFS clones, one thread, with `.git`) | 288 s | for reference |

## 9. Migration and next steps

1. **Old state to objects:** `layr objects import <state>` for each state, oldest first, with
   `--cold` for history and hot for the heads of lines. Verify each state by a checkout and a
   compare (`layr objects verify`), or cheaper, by writing the tree again from the old storage
   and comparing the root ids: the writer is deterministic, so equal folders give equal ids.
2. **Records name trees:** a state record gets a `tree` field. The state id becomes the hash of
   the record; checks and attestations name the tree id.
3. **Packs** for cold objects and trees; a pack index like `layr_store::chunks::Index`.
4. **Reads from objects:** `show`, `diff`, `log` and `try` read trees; a line folder is a
   checkout of hot blobs. Then snapshots stop being the truth.
5. **Transfer:** push and fetch send the objects that the other side lacks (tree walk with
   known ids); cold chunks match the chunk store of the remote.
6. **Garbage collection:** mark from the trees of live records; sweep loose files and repack.

Open questions:

- **Secret keys:** where the command reads the project identity for keyed ids, and how a project
  that changes mode re-keys its objects (all ids change).
- **Checkout as the caller:** on a root service, writing the folder of a non-root caller needs the
  caller's file identity (`privs::as_caller_fs`) for writes while the store is read as root.
- **Owners and times:** are times part of a state? Leaving them out gives dedup and stable ids;
  build tools that compare times see new times after a checkout.
- **Flags and ACLs:** macOS file flags and ACLs, and POSIX ACLs on Linux, are not kept.
- **Hot or cold:** which blobs stay hot (heads of lines only, or the last N states) and when a
  hot blob turns cold.
