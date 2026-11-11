# Object store v2

Status: experimental. This file is the **single definition** of layr object ids, object
encodings and packs. The crate `crates/layr-objects`, the command `layr objects`, the Layr Cloud
server (`docs/api.md` in ifdotpy/layr-cloud) and the V1 client use it. A change to a byte layout
here changes the format: the test vectors (section 12) and the unit tests that pin them change too.

Snapshot storage is v1. Object storage is v2.
`layr migrate` moves a project from v1 to v2 (section 13, user guide `docs/migrate.md`).
Experimental object configs marked `v:1` upgrade automatically; object encodings and IDs stay unchanged.
After migration, source states remain usable without native state snapshots.
Status, diff, and file reads use object trees directly.
Operations that require native folders can create temporary copies.
New saves write objects before they publish the state record.
Cache and derived states keep their native format. Owner decision 2026-11-11: a cache is an
ordinary overlay (a glass), and its states go into v2 objects like any glass; a state record then
names one tree and one times table per glass ([cache overlays](cache-overlays.md) section 2).
Remote compatibility is separate (section 13 and `docs/remote-format.md`).

## 1. Model

- A **blob** is the bytes of one file. A **tree** is one folder: its entries with their metadata
  and the ids of their blobs and trees. Trees form a Merkle tree: the id of the root tree names
  the complete content of a folder.
- A **times table** holds the modification time of every entry of a tree. It is not part of the
  tree, so two folders with the same content have the same tree id whatever their times.
- A **state** names a tree and a times table (section 8). The id of a state is the hash of its
  record. Checks and attestations name the tree id, so evidence for a tree holds for every state
  with that tree.
- The id of an object names its logical content. The store may keep it whole, as a zstd frame,
  as a delta, as chunks, or in a pack: the id stays the same.

## 2. Object ids

```text
plain id = SHA-256( type " " decimal-length "\0" content )
keyed id = HMAC-SHA256( key, type " " decimal-length "\0" content )
```

`type` is one of the ASCII words below, `decimal-length` is the content length in bytes in ASCII
decimal without leading zeros, and `\0` is one zero byte. An id is 32 bytes; its text form is 64
lowercase hex digits.

| type | content | made by |
| --- | --- | --- |
| `blob` | the bytes of a file | every write |
| `tree` | a tree encoding (section 3) | every write |
| `times` | a times table (section 4) | every write |
| `chunk` | a content-defined part of a large blob (section 5) | cold writes |
| `opaque` | bytes whose structure the id does not describe: a pack data file (section 6), an encrypted object of a Secret overlay | packs, transfer |
| `chunk-pack` | a pack of content-defined chunks of a cache layer | V1 client |
| `git-pack` | a Git pack of imported commits | V1 client |

The last two types belong to the Layr Cloud V1 client (ifdotpy/layr#423, `docs/remotes.md`,
section "Layr Cloud API v1"); they use the id rule of this section unchanged. An upload names the
type in the header `Layr-Object-Type`. The V1 client sends states as trees, times tables and packs
of this file (section 14).

- **SHA-256** is the persisted content hash of layr (`layr_hashers::ContentAddress`). It runs on
  the ARMv8 SHA extensions and SHA-NI; a second write of Chromium, which hashes every file
  (4.5 GB), takes 33 s.
- The type is part of the hashed bytes, so a blob and a tree with equal bytes are different
  objects. The header is the header of Git's SHA-256 object format: **a layr blob id equals the
  Git SHA-256 blob id** of the same bytes (`git hash-object` in a `--object-format=sha256`
  repository gives the vectors of section 12). Trees differ from Git trees.
- **Checking an id needs its type.** A reader recomputes the id from the type and the content. A
  transfer (an upload, a download, a pack entry) therefore carries the type with the content. A
  server can check every plain id; it cannot check a keyed id.
- An object that is stored compressed or encrypted keeps the id of its content; a stored file
  that is not an object (a pack data file, an encrypted pack) is named by the `opaque` id of its
  bytes.

### Secret overlays

The mode is a property of each overlay (`docs/cloud-rfc.md` 7.0). Public and Private overlays use
plain ids: equal files have equal ids in every project, and a server checks and dedups them. A
Secret overlay uses keyed ids, so equal content gets equal ids only inside that overlay and key
epoch, and a server cannot test whether the overlay holds a known file:

```text
key = HKDF-SHA256( ikm  = the age identity of the overlay's key epoch, as its text "AGE-SECRET-KEY-1...",
                   salt = the epoch id, as UTF-8,
                   info = "layr-objects-id-v1", length = 32 )
```

The identity comes from the epoch's slots (`src/store_remote/epochs.rs`, `overlays.rs`
`layer_identity`). A new epoch gives new ids: objects written in an older epoch keep their ids,
and dedup works inside one epoch. Code: `layr_objects::id::epoch_key`. A store records its id
function in `config.json` and refuses another one.

In this slice `layr objects` writes plain ids: Secret overlays (the `mode` property of
`overlay.create`) do not exist in layr yet. When they land, `layr objects` opens the store of a
Secret overlay with `Keying::Keyed(epoch_key(identity, epoch))`; no other change is needed.

## 3. Tree encoding

Numbers are unsigned LEB128 varints in their shortest form (a decoder refuses a longer form).
`bytes` is a varint length and then the raw bytes.

```text
tree    = version:u8 = 0x01   count:varint   entry*count   groups:varint   group*groups
entry   = name:bytes   type:u8   mode:varint   body   xattrs:varint   (xname:bytes xvalue:bytes)*xattrs
body    = type 0x01 file:     size:varint   blob-id:32 bytes
          type 0x02 folder:   tree-id:32 bytes
          type 0x03 symlink:  target:bytes
group   = paths:varint   path:bytes*paths
```

Rules (a decoder checks each one and refuses the tree otherwise):

- Entries are sorted by name bytes (unsigned byte order), names are unique. A name is not empty,
  not `.` or `..`, and has no `/` and no zero byte; any other bytes are allowed, so non-UTF-8
  names stay as they are.
- `mode` is `st_mode & 0o7777` (permission bits with setuid, setgid, sticky). A symbolic link has
  mode 0. A symbolic link target is not empty and has no zero byte.
- An empty folder is a folder entry whose tree is the empty tree (`01 00 00`).
- Extended attributes are sorted by name bytes, names unique, not empty, no zero byte. A tree keeps
  every attribute except `com.apple.provenance`, names that start with `com.layr.`, and the classes
  `trusted.`, `security.` and `system.` (SELinux labels, file capabilities, POSIX ACLs and overlayfs
  internals belong to the machine). A decoder refuses those names too.
- **Hard links:** each group lists the paths (relative to this tree, `/`-separated, sorted, at
  least two, each component a valid name) of one file with more than one name in the folder.
  Groups are sorted by their first path. Only the root tree of a write has groups, so trees below
  the root are pure content and are shared between states. Each path names a regular file of the
  tree and is in one group only (a checkout refuses other trees).
- Not in a tree: owners, times (section 4), file flags, ACLs, and the mode and attributes of the
  root folder itself.
- No bytes after the last group. `encode(decode(x)) == x` for every accepted `x`.

A FIFO, a socket or a device in a folder stops a write with an error.

## 4. Times table

One table per folder, like the trees. A state names the table of its root folder.

```text
times  = version:u8 = 0x02   own:zigzag-varint   count:varint   record*count
record = 0x00 delta:zigzag-varint     a file or a symbolic link
       | 0x01 id:32                   a folder: the id of its times table
```

- Records follow the entries of the folder's tree, in tree order (sorted by name bytes), one for
  each entry.
- A time is the modification time in **nanoseconds** since the Unix epoch, as a signed 64-bit
  number (seconds x 10^9 + nanoseconds). `own` is the time of this folder, zigzag-encoded
  (`(v << 1) ^ (v >> 63)`). `delta` is the entry's time minus the previous file time of this table
  (the first: minus `own`), zigzag-encoded.
- Files that share a time cost two bytes each.
- A save writes the tables on the way to a change and shares all the others, whatever the size of
  the tree. Equal folders with equal times share a table across states.
- To send a state, walk the tables from its root table (`Store::times_ids`): the root id alone does
  not reach them.

The local per-folder format uses version 2. Cloud V1 uses its original flat version 1 table. On transfer, Layr converts between the two. A local version 1 state is checked against both older forms and converted. If both forms match but describe different times, Layr refuses the state.

A checkout with a times table sets every modification time: the entries of a folder first, then
the folder (a new entry changes the time of its folder). The access time is left as it is. Build
tools that compare times (cargo, ninja, make) then see an unchanged tree (section 15).

## 5. Stored forms of one object

A store keeps an object in one of these forms. Every form decodes to the content, and every read
checks the content against the id.

| form | where | bytes |
| --- | --- | --- |
| hot | `blobs/` | the exact file bytes; a clone of the source (`clonefile`, `FICLONE`), no extended attributes |
| loose | `objects/` | `"LOB1"  type-byte  form-byte  body` (below) |
| packed | `packs/` | an entry of a pack (section 6) |

Type bytes: `b` blob, `t` tree, `m` times, `c` chunk, `o` opaque. Loose bodies:

```text
form 'z' whole:    one zstd frame with its content size (zstd level 3)
form 'y' whole:    dict-id:32   one zstd frame (level 3, content size) made with the store dictionary
form 'd' delta:    depth:u8   base-id:32   zstd frame that decodes with the base content as a raw prefix
form 'c' chunked:  count:varint   (chunk-id:32   length:varint)*count
```

- **Delta:** a new version of a file against the blob at the same path in the previous tree, as
  `zstd --patch-from` makes it (window covering both inputs, long-distance matching). Both versions
  are at most 64 MiB. A decoder gives the base content to zstd as a raw-content prefix
  (`ZSTD_DCtx_refPrefix`). The writer keeps a chain at most 16 deltas deep and stores the next
  version whole; a reader refuses a chain deeper than 255.
- **Chunked:** a cold file of 1 MiB or more without a delta base is cut with the FastCDC chunker of
  the cache remote (`layr_store::chunks`: 16 KiB minimum, 64 KiB average, 256 KiB maximum, fixed
  gear table). Each part is a `chunk` object, so equal parts of large files are stored once.
- **Dictionary:** a whole blob of at most 64 KiB is written in form `y` once the store has a
  dictionary. The first repack trains it (zstd's trainer, 112 KiB, from up to 16 MiB of whole blobs
  of at most 64 KiB in pack order) and stores it as `dicts/<id>`, where the id is the `opaque` id of
  the dictionary bytes. A store keeps its first dictionary. A reader of a `y` object needs that
  dictionary; a pack never needs it (packs hold raw content).
- Delta bases by similarity (a renamed file against a blob with the same name or extension and a
  near size, `WriteOptions::similar`) were measured and are off: they saved 0.05 % (section 15).

## 6. Packs

A pack holds many objects in zstd blocks; the writer makes blocks of about 4 MiB. **The same pack format is used locally and on remotes**
(S3 stores and Layr Cloud): a repack writes packs, and a push sends them as they are.

```text
<pack-id>.pack   "LPK1"   block*
<pack-id>.idx    "LPI1"   pack-id:32
                 block-count:u32    (offset:u64  compressed:u32  plain:u32)*
                 entry-count:u32    (id:32  type:u8  form:u8  block:u32  offset:u32  length:u32)*
                 checksum:32
```

- Integers are little-endian. A block is one zstd frame with its content size; the blocks follow
  the magic without gaps, and the data file ends after the last block.
- The pack id is the `opaque` id of the whole data file. The index checksum is the SHA-256 of
  every index byte before it.
- Entries are sorted by id, ids are unique, and each entry lies inside its block's plain bytes.
  `type` is a type byte (section 5). `form` is `r` (raw: the content of a tree, times table, chunk
  or whole blob), `d` (a delta body as in section 5) or `c` (a chunk list).
- The writer cuts a pack into blocks of about 4 MiB of plain bytes, one zstd frame each, at level
  19. A reader accepts any number of blocks. A read decodes the block of its entry, and a store keeps
  up to 512 MiB of decoded blocks. A block is the unit of a cold read: 4 MiB against one stream per
  pack makes a read of one file 13 times faster for 4.2% more bytes (section 15).
- A reader opens an index without decoding it and searches its sorted entries in place, so opening a
  store costs nothing per object. The index checksum is checked when a pack is received; every
  object read is checked against its id.
- A pack holds about 56 MiB of plain bytes, so a compressed pack stays below 64 MiB, the largest
  single upload of Layr Cloud. A repack compresses one pack per thread.
- Order inside a pack: the trees and times of the imported states first (they hold ids, which do
  not compress), then blobs and chunks in path order (files of one folder share blocks), then other
  objects.
- Packs use no dictionary: in a stream of many files it saves nothing (section 15); the store
  dictionary serves loose frames, which compress one file at a time.
- **Secret overlays:** the data file and the index are each encrypted with age to the recipient of
  the overlay's key epoch before they leave the machine, and the server names each encrypted file by
  the `opaque` id of its encrypted bytes. The entries inside keep their keyed ids.

A repack writes a pack, checks that each block decodes to its input, writes the data file and then
the index, checks the data file on disk against the pack id, and only then removes the loose files
that the pack holds. A crash leaves at most a loose copy too many, which the next repack removes.

## 7. Local layout

```text
<project>/meta/objects/
  config.json             {"ids":"sha256","v":2}   (or "hmac-sha256")
  blobs/ab/cdef...        hot blobs
  dicts/<id>              the store dictionary (section 5)
  objects/ab/cdef...      loose objects
  packs/<id>.pack .idx    packs
  states/<state id>       {"times":"<id>","tree":"<id>"}
  tmp/                    files being written
```

The first two hex digits of an id name a fan-out folder. A write checks for the object, writes a
temporary file, and renames it with `RENAME_NOREPLACE` (`renameatx_np(RENAME_EXCL)` on macOS; a
link and an unlink elsewhere). An existing name means the content is there: nothing is rewritten,
and concurrent writers agree. A failed write removes its file; opening a store removes temporary
files of processes that are gone.

## 8. States

A state names its tree and its times table: `{"times":"<id>","tree":"<id>"}` (keys sorted, no
spaces). Locally it is the file `states/<state id>`; on Layr Cloud it is the body of
`PUT /states/<state>`.

Note for the API contract (layr-cloud `docs/api.md`): `times` is a required field of
`PUT /states/<state>`, an object id like `tree`. The server refuses a body without it, and both
objects must be linked to the project before the state is.

Owner decision 2026-11-11: in the overlay model a state names one tree and one times table per
glass (overlay), the base included. This file does not define that layout yet; the format above
is what is built.

## 9. Hot and cold

- **Hot** blobs are whole files, so a checkout clones them (no data is copied). The hot set is the
  blobs of the head of each line and of the states before it on its first-parent chain,
  `objects.hot-states` states in all (default 5, the size of the undo ring).
- Every other blob is **cold**: a zstd frame, a delta or chunks, in a pack after a repack.
- A repack turns hot blobs outside the hot set into cold objects; a hot file goes only after its
  cold form reads back to the same id. Age is no rule of its own.
- Hot blobs are local only: a push sends packs, never `blobs/`.

## 10. Writes and repack

Two steps, as Git does with loose objects and `git gc`:

1. **Save:** an incremental write reads only the paths that changed since the parent state
   (`write_changed`) and copies everything else from the parent's tree and times table. New blobs
   are hot (clones) or cold loose objects at zstd level 3: a delta against the blob at the same path
   when there is one, else a frame with the store dictionary for a small file. A save of a few files takes milliseconds
   (section 15).
2. **Repack** (`layr objects repack`, also run by `layr gc` when a project has an object store):
   hot blobs outside the hot set turn cold, the first repack trains the store dictionary, then every
   loose object goes into packs (blocks of about 4 MiB, level 19).

## 11. Large folders (next step)

A tree lists every entry of its folder, so a change in a folder of 100,000 entries writes a tree of
about 6 MB. The next format version shards large folders:

- A folder with more than 4,096 entries is stored as a **prolly tree**: entries in name order are
  cut into nodes at content-defined boundaries (a rolling hash of the entry bytes, average 1,024
  entries per node), and an index node lists the nodes with their first names.
- Equal folders give equal nodes (the cut points depend only on the entries), a change rewrites
  about one leaf and the index path, and a lookup reads one node per level.
- A prolly tree beats a HAMT here: it keeps name order (a checkout and a diff walk in order) and
  needs no rebalancing.
- Encoding: a new tree version byte `0x02` with node references; version `0x01` trees stay valid.

Not implemented in this slice: the largest folder of Chromium (`chrome/app/generated_resources_grd`)
has 3,359 entries, and all 47,031 trees of Chromium together are 26 MB.

## 12. Test vectors

The unit test `format_test_vectors` pins every line below.

```text
blob ""                 473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813
blob "hello\n"          2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4
chunk "abc"             ada203fc3bdbffd43c38aae4e8af47e642b35cf6d7985e81a29483c746db3309
opaque ""               2b528cd00f2cb0427c1a0f8b5cc0c511032d28d1383fa0639ce3b13225c78132
empty tree   bytes 010000
             id    463e15cd99aa5c788c292f8d1c5e510ac9c2b9f79817c06c2457f0dcf3931242
```

A tree with a file `a.txt` (mode 0644, content `hello\n`, attribute `user.k` = `v`), an empty
folder `bin` (mode 0755) and a symbolic link `link` to `a.txt`:

```text
01                                  version
03                                  3 entries
05 612e747874                       name "a.txt"
01 a403                             file, mode 0o644 (420)
06 2cf8...ebb4 (32 bytes)           size 6, blob id of "hello\n"
01 06 757365722e6b 01 76            1 attribute: "user.k" = "v"
03 62696e                           name "bin"
02 ed03                             folder, mode 0o755 (493)
463e...1242 (32 bytes)              the empty tree
00                                  no attributes
04 6c696e6b                         name "link"
03 00                               symbolic link, mode 0
05 612e747874                       target "a.txt"
00                                  no attributes
00                                  no hard link groups

bytes 010305612e74787401a403062cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9eb
      b40106757365722e6b01760362696e02ed03463e15cd99aa5c788c292f8d1c5e510ac9c2b9f79817c06c2457
      f0dcf393124200046c696e6b030005612e7478740000
id    9e1e4e5228345fd99d1edcd9c3c6f946dcc323909dcdfbee7c6166869e1e41ae
```

Its times tables: root and `a.txt` and `link` at 1,700,000,000,000,000,000 ns, `bin` 1 ns later:

```text
times of bin
bytes 02 8280d0e2c6bfce972f 00
id    bd8e0715e895a66d44e355ee054bd1a6fdc7b0078bf13675bbba548ee832bb0a

times of the root
bytes 02 8080d0e2c6bfce972f 03  00 00  01 bd8e0715e895a66d44e355ee054bd1a6fdc7b0078bf13675bbba548ee832bb0a  00 00
id    659359727843b1f863d5e35780e7afb691ce30818fc86e68e7f561344d53b530
```

Keyed ids, with the identity `AGE-SECRET-KEY-1EXAMPLE` and the epoch `epoch-1`:

```text
key                     975c2d8e41c44ccf4aea50a2294bdd7b95ad5fab019182e8d24193e2951c398b
keyed blob "hello\n"    ed428789bac0bafb394f8234a3fce8cc1069cf68a7dede6d0338ef3033971447
```

zstd output is not pinned: any valid frame that decodes to the content is correct.

## 13. Commands and migration

```text
layr objects import <rev> [--cold] [--full] [--parent <tree>] [--json]
layr objects verify <rev> [--cold] [--full] [--json]
layr objects repack [--json]
layr objects stat [--json]
layr objects checkout <tree or rev> <folder>
```

- `import` writes the tree and times of a state (read through `Repo::state_root`, as `layr export`
  reads it). When the first parent was imported, it reads only the paths that the backend lists as
  changed (`changes::candidates`); `--full` reads everything. `--cold` stores new blobs compressed,
  as deltas against the parent's tree. Cache and derived states are refused.
- `verify` imports, checks out the tree with its times into the project's work folder, compares
  every entry, byte and time with the state, and removes the copy. Exit 1 on a difference.
- `repack` evicts hot blobs outside the hot set and packs loose objects.
- `checkout` makes a folder with the times of the state. It needs the `export` right and the
  secret check of `layr export`. On a root service every write of the checkout runs with the
  caller's file identity (`privs::CallerFs`: file system uid, gid and groups, one step at a time on
  one thread), while the store is read as root: every file belongs to the caller, and root never
  writes into the caller's folder (`tests/t201-objects-service.sh`).
- `import`, `verify` and `repack` need the admin role; `stat` needs read access.

Open questions: sharded trees (section 11); a repack that merges small packs; garbage collection of
objects that no state names.

### Migration

```text
layr migrate [<project>] [--dry-run] [--resume] [--delete-old]
```

The steps for users are in `docs/migrate.md`. Code: `src/cmd/migrate.rs`, `src/objstore.rs`.

1. **Refusal.** The run stops with exit 1 while a command holds the project lock or a line lock
   (a running command or agent), and on macOS while the user daemon serves the store (its follow
   passes, syncs and gc write). On Linux only the locks count: the service socket there can be the
   service that runs the command. A run that did not finish (`meta/migrate/running`) is continued
   only with `--resume`.
2. **Unsaved work.** Each line folder of this machine is saved as an automatic state, as any layr
   command that saves the working folder first (`Repo::working_state`, message "before layr
   migrate").
3. **Evicted states.** A state whose snapshot retention removed (`gc.evict`) is read back from a
   store remote before the run takes the lock. A state that no remote gives stops the run with a
   list; nothing is changed then. A state imported from Git without a tree is built from Git, as
   any read builds it.
4. **Import.** Under the project lock (other commands wait), every source state, parents first and
   older first, is imported as `layr objects import` does: incremental against the first parent
   when the parent has a snapshot and a tree, else a full read. The heads of the lines and the
   states before them on the first-parent chain (`objects.hot-states`, default 5: the head and 4)
   are hot; every other state is cold, as deltas against the first parent's tree. Cache and derived
   states stay snapshots. Freed states have no tree to move.
   A replay can clear the deletion flag of an absent state without an owner.
   Migration reports these states and keeps their records unchanged.
   It checks both the stored owners and the owners from record replay before it excludes an absent source.
5. **Verify.** Each state is checked out with its times into the work folder and compared with its
   snapshot (`compare_with`: names, types, permission bits, bytes, link targets, kept extended
   attributes, hard link groups, modification times). The first difference stops the run with exit
   1; nothing is deleted.
6. **Mapping table.** A verified state is appended to `meta/migrate/map` (a header line
   `layr-migrate-map 1`, then `<state id> <tree> <times>` per line, synced). `--resume` skips the
   states in it. A last line without a newline is a write that a crash cut: it is ignored and
   removed by the next append.
7. **Records.** Every 64 verified states, and at the end, one signed record with the operation
   `state.tree` gives them their fields:

   ```json
   {"states": {"<state id>": {"times": "<id>", "tree": "<id>"}}}
   ```

   The state keeps its id and its signed save record; the derived row and `StateRec` get `tree` and
   `times`. A later record of the same state replaces both (order key: time, machine, sequence).
   Line heads, refs, tags, parks, queue entries, changes, attestations, approvals and the Git
   mapping name state ids and stay as they are. A successful migration enables `objects.enabled`.
   Later source saves write their portable tree and times before the new state record is signed.
   Source reads use objects directly. Native source copies remain only while a consumer needs them.
8. **Repack** (`layr objects repack`): hot blobs outside the hot set turn cold, loose objects go
   into packs.
9. **Old storage.** Without `--delete-old` the snapshots stay and the run prints what
   `--delete-old` frees. With it, after every source state is recorded, the snapshots of the
   recorded states are checked again against a complete object checkout before any snapshot goes
   (`drop_trees`: reserved against pins, moved to the trash, marked absent),
   except active macOS view bases, states that an overlay names, and APFS layers that another native layer needs.
   Line heads and source states with cache references can lose their native source trees.
   Cache and derived states keep their separate native representation.

Reads after the migration:

| reader | source |
| --- | --- |
| `layr show <rev>:<path>` | the trees and blobs (`objstore::entry`); no snapshot is read or made |
| `layr line new <name> <rev>` of a state without a snapshot | a checkout of the tree with its times (hot blobs are clones) |
| `state_src` readers, including status, diff and file reads | direct tree metadata and blob reads; individual native entries only when a file descriptor is required |
| native-only consumers, including the merge engine and v2 remote export | a disposable native tree from `Repo::state_root` |

`ObjectTree` supplies metadata, files, links, and directory traversal on all three backends.
On macOS, a thin layer can use an object tree as its base.
On Linux, `Source` selects a native folder or an object tree.

Object writes finish before record publication. The `meta/object-native/<state>` marker queues removal of each new native source copy.
Command cleanup checks the committed record, reserves the tree against readers, and removes the native copy.
A crash leaves the marker for the next command. A pin or native layer dependency postpones removal.
Native trees reconstructed for older consumers use the same queue.

The local object store must contain the root tree and times table before a reader selects objects.
Missing or corrupt descendants cause a read error. They do not trigger a fallback to an older snapshot.
Disabling `objects.enabled` stops future object writes. Existing object states still require `meta/objects`; deleting it is not a rollback.

**Remotes.** Local migration preserves each configured remote. Layr Cloud V1 sends portable
`tree`/`times` states and typed objects (section 14). File and S3 store remotes retain their
store-v2 transfer contract. Native-only transfer paths use disposable copies from the objects.
Migration does not rewrite an existing remote in place.

## 14. The two tree formats

Decided (Layr and LayrFS, 2026-11-07): `tree` wins. The Layr Cloud V1 client (#423) sends states as
`{"times","tree"}` with packs of this file; `part` and `state-tree` are gone. The comparison below is
kept as the reason.

| | `tree` (this file) | `state-tree` (V1 client, store format v2) |
| --- | --- | --- |
| shape | one object per folder, a Merkle tree | one flat JSON map of every file path |
| names content by | blob ids (content only) | frame links into `part` objects (storage position) |
| same content, other storage or push | same id | another id |
| folders, empty folders | kept | not kept |
| extended attributes, hard link groups | kept | not kept (links only inside a part) |
| times | beside the tree (`times`), not in the id | inside the tree, in the id |
| change of one file in a big tree | rewrites the folders on its path | rewrites the whole map |
| signed | by the record that names it | header signature inside the object |

What V1 would change to use `tree`: the state body becomes `{"times":"<id>","tree":"<id>"}`; a
push sends the trees, times tables and packs (section 6) that the server lacks (`POST
/objects/missing` with tree, times and pack ids); the server finds a blob through the pack indexes
(or reads them to build its own index). `part` and `state-tree` then go away for Public and Private
remotes; `chunk-pack` and `git-pack` stay as they are.

What `tree` would change to serve as the V1 state tree: nothing in its encoding, but it does not
hold storage positions, so a reader needs pack indexes to find bytes. The reverse (the V1
`state-tree` as the only tree) would lose content addressing of folders, folders and attributes,
and stable ids across pushes; this file does not recommend it.

## 15. Measurements

macOS 27.2, M5 Max (18 cores), APFS, release build of `examples/objects-bench` (the crate's bench;
its usage is at the top of its source). Other agents used the machine at the same time, so times
are upper bounds. Bytes are file sizes; "allocated" counts 4 KiB blocks (`du`).

A cache glass (real `target` states, 2026-11-11) is measured in
[cache overlays](cache-overlays.md) section 21: about 2.3 to 2.6 GB compressed per new build, kilobytes per state of the same build.

### Choices

The compression choices come from two measurements: the standalone bench of the Layr session
(`benchmarks/compression-20261107/`: program and `results-20261107.txt`; files one by one and as
one stream, on layr, layr-studio and a Chromium sample) and this crate's bench on the real objects
of the stores below.

| question | measured | choice |
| --- | --- | --- |
| save level | level 3 costs 0.03 ms per 5 KiB file, level 19 about 1 ms (benchmark); a save of 1 or 10 files takes milliseconds (below) | level 3 |
| dictionary for files one by one | 11 to 13 % smaller at level 3, 16 to 20 % at level 19, on files below 64 KiB (benchmark); 9.0 % on the whole blobs of the history below | loose small blobs use the store dictionary |
| dictionary in packs | 0.4 to 0.5 % larger in 1 MiB blocks of small sources; larger in 1, 4 and 8 MiB blocks of the history and in 4 MiB blocks of Chromium (this crate's matrix, below) | none in packs |
| delta against the previous version | 32 times smaller than a whole level-19 frame on 213 changed files (benchmark) | saves write deltas |
| delta base by similarity (keep at 5 % or more) | 0.04 % on the history below (14 renames) | same path only |
| pack compression | one stream is 18.7 to 27.6 % of raw against 26.5 to 31.2 % file by file at level 19 (benchmark); on Chromium one stream per pack is 4.2 % smaller than 4 MiB blocks, but a cold read is 13 times slower (see "Reads, block size and saves") | level 19, blocks of 4 MiB |
| long-distance matching | 0 to 3 % on large streams (benchmark); nothing below the 8 MiB window | on above 8 MiB |
| level 9 | slow and a small gain | not used |

### History of this repository

The last 60 first-parent commits of main (adb4a7c2 to 6ada771), each written cold against the tree
of the one before, from `git archive` folders. A state is about 652 files, 8.6 MiB.

| store | bytes | allocated |
| --- | --- | --- |
| loose after the saves, without a dictionary | 3,282,180 | 6.5 MiB (1,235 files) |
| of which 674 whole blobs (the first tree, renamed and new files) | 2,637,818 | |
| of which 217 later versions as deltas | 93,613 | |
| of which trees / times tables | 537,605 / 13,144 | |
| loose after the saves, with the store dictionary | 3,044,131 (whole blobs 2,399,769) | 6.4 MiB |
| loose with delta bases by similarity | 3,280,708 | 6.5 MiB |
| after `repack`: one pack (one stream, 12.7 s) and the dictionary | 1,918,764 + 114,688 | 2.0 MiB |

All 60 states check out from the pack, and from the store with the dictionary, with their times,
and compare equal (entries, bytes and times) to their folders.

This crate's matrix in 4 MiB blocks, on the earlier 60-commit window (ba25720 to e900d3f, 1,241
objects, 9.6 MB of plain payload; bytes include the 112 KiB dictionary):

| level | no dictionary | dictionary |
| --- | --- | --- |
| 3 | 2,591,227 | 2,747,662 |
| 9 | 2,262,497 | 2,541,875 |
| 19 | 2,056,060 | 2,280,991 |
| 19+long | 2,055,748 | 2,253,802 |

At level 19 without a dictionary, block sizes gave 1 MiB 2.34 MB, 4 MiB 2.06 MB, 8 MiB 1.94 MB,
one stream 1.86 MB (the same earlier window).

### Chromium

`src` at efd35c5: 516,265 files, 47,031 folders, 4.47 GB of file bytes (`.git` left out).

| step | time | store |
| --- | --- | --- |
| hot write (clones) * | 149 s | blobs are clones: no data blocks copied |
| hot write again * | 33 s | 0 new objects |
| checkout with clones * | 78 s | 516,265 clones, compare equal |
| cold write (save form, no dictionary yet) | 171 s | 1.62 GB loose, 3.1 GiB allocated (536,155 files) |
| repack, 4 MiB blocks (first try) | 681 s | 73 packs, 1.407 GB |
| repack, one stream per pack (the format) | 683 s | 73 packs, 1.350 GB and a 112 KiB dictionary, 1.26 GiB allocated |

\* From the first round of this slice (before times tables and packs). The two repacks of this
table ran without named states, so their objects were in id order; see the next section.

### Reads, block size and saves

Measured with `objects-bench` and `objects-read`; scripts and raw output are in
`benchmarks/objects-20261107/`. Chromium `src` as above, load average 13 to 80. A cold read opens
the store anew for each read, as a separate command does.

The order of objects in a pack: with named states (path order) the packs of Chromium are 1.155 GB
against 1.350 GB in id order, 14 % smaller. A store without named states also reads badly: in id
order and one stream per pack, a checkout took 6,453 s and decoding every blob ran at 2 MB/s,
because nearly every read decoded another 56 MiB block.

Block size, in path order, times table still one object for the whole tree:

| block | pack bytes | cold read of one blob, median | save of 1 file | save of 10 files | checkout | decode a tree |
| --- | --- | --- | --- | --- | --- | --- |
| one stream (up to 56 MiB) | 1,155,233,429 | 36.1 ms | 93 ms | 110 ms | 233 s | 51 MB/s |
| 4 MiB | 1,203,893,968 | 2.7 ms | 38 ms | 65 ms | 109 s | 1,279 MB/s |
| 1 MiB | 1,253,485,248 | 0.8 ms | 35 ms | 63 ms | | |

The save columns in this table re-store unchanged files, so they write little. 4 MiB is the
format's choice: 4.2 % more bytes than one stream for a read 13 times faster; 1 MiB costs twice that
and gives a save almost nothing.

The other costs of a save and of a read were the decoding of all pack indexes when a store opens
(74 ms) and the one times table of the whole tree (17 ms to decode, 3 ms to encode, 24 ms to compress
and write, per save). With indexes read in place and one times table per folder (the format now):

| Chromium | before | now |
| --- | --- | --- |
| open the store | 74 ms | 2.25 ms |
| open and read one old blob, median | 77 ms | 7.0 ms |
| decode a tree, in parallel | 1,279 MB/s | 1,302 MB/s |
| save of 1 changed file: first, then median of 5 | | 22.7 ms, 8.2 ms |
| save of 10 changed files: first, then median of 5 | | 121 ms, 33.0 ms |
| pack bytes, 73 packs, 583,186 objects | 1,203,893,968 | 1,210,079,112 |
| walk all 47,032 times tables, one thread | | 564 ms |

Each save here appends a line to the files and writes from the previous state. On the 60 states of
this repository: pack 2,021,599 bytes (1,803,616 with one stream and one times table per state);
checkout of the 60 states 7.24 s, the slowest 0.17 s; a cold read 3.2 ms, 28 of the 100 sampled
blobs through deltas; decode 870 MB/s.

Deltas and packs: with one stream per pack, the same 60 states gave a pack of 1,803,616 bytes with
deltas and 1,772,064 without, so a pack finds the repeats by itself. Deltas matter before a repack:
3,240,029 loose bytes against 5,839,574.

A checkout of Chromium from its packs with the per-folder tables took 106.6 s (`cp -cR` of the same
tree: 288 s), and every entry, byte and time compared equal to the source (26.6 s).

This crate's matrix on the 536,155 objects (4.28 GB of plain payload, 1,038 blocks of 4 MiB, 18
threads):

| level | dictionary | bytes | ratio | compress MB/s | decompress MB/s |
| --- | --- | --- | --- | --- | --- |
| 3 | no | 1,533,926,035 | 0.358 | 808 | 3,294 |
| 3 | yes | 1,541,623,574 | 0.360 | 733 | 4,106 |
| 9 | no | 1,444,348,411 | 0.337 | 249 | 3,493 |
| 9 | yes | 1,462,262,304 | 0.342 | 220 | 4,225 |
| 19 | no | 1,381,458,876 | 0.323 | 9 | 2,913 |
| 19 | yes | 1,410,303,955 | 0.329 | 18 | 3,168 |
| 19+long | no | 1,381,333,057 | 0.323 | 11 | 4,256 |
| 19+long | yes | 1,404,309,539 | 0.328 | 18 | 2,917 |

### Save cost

An incremental write (`write_changed`) of this repository's working folder (687 files, hot), as a
whole process with store open, five runs each: 1 changed file 2.2 to 28 ms (median 11 ms), 10
changed files 2.6 to 43 ms (median 3.6 ms). A full write of the same folder takes 290 to 460 ms.

### Times and cargo

This repository's working folder (built), written cold, checked out to a new folder, with its
`target/debug` folder cloned next to it, then `cargo build`:

```text
checkout with the times table:     Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.74s
checkout without the times table:  Compiling layr-hashers v0.1.0 (.../cold-plain/crates/layr-hashers)
                                   Compiling layr-adapter v0.1.0 (.../cold-plain/crates/layr-adapter)
                                   Compiling layr v0.38.1 (.../cold-plain)
                                   ... (stopped)
```

(A checkout of hot blobs without the table keeps the times of the store files, because a clone
keeps the time of its source; a checkout always applies the table.)
