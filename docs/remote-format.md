# Store format v2

The decisions in `remote-design.md` define this format.

## Objects

- `store.json`: `{"format":2,"project":"<UUID>","recipient":"age1..."}`.
- `slots/paper.age`: the vault identity, encrypted to a paper identity.
- `slots/machine-<UUID>.age`: the vault identity, encrypted to the existing machine key as an age `ssh-ed25519` recipient.
- `records/<machine UUID>/<first>-<last>.age`: exact signed JSONL records. Sequence numbers have 20 decimal digits.
- `states/<state UUID>/<attempt>.tree.age`: a header line and one zstd frame, encrypted with age.
- `states/<state UUID>/<attempt>.<part>.data.age`: concatenated zstd frames, encrypted with age. Parts have four decimal digits.
- `git/<sha256>.pack.age` and `git/<sha256>.commits.age`: Git objects of imported commits (section "Git packs").

An attempt has eight lowercase hexadecimal digits from the operating system random source.
The first complete attempt in name order represents a state.
A tree object is the commit point. All its data parts must exist first.

The part size target is 16 MiB. A larger frame has a separate part, same as Desk v1.
S3 objects above 256 MiB use multipart upload. Multipart completion uses `If-None-Match: *`.
All other object writes also use `If-None-Match: *` or atomic exclusive publication for a folder.
No object is replaced. There are no mutable store refs.

## Git packs

An imported state (`import` kind, `docs/ownership.md`, "States imported from Git") may have no tree
on the machine that pushes. For such a state the Git pack below replaces the tree: the store holds
the Git objects, and a clone builds the state from them offline. A push therefore also writes the Git
objects of the imported commits that the remote does not list yet:

- `git/<sha256>.pack.age`: the objects of those commits, as one `git pack-objects` pack, encrypted.
  The name is the SHA-256 of the unencrypted pack.
- `git/<sha256>.commits.age`: `{"format":1,"commits":["<40 hex>", ...]}` for the commits the pack adds,
  encrypted. It is written after the pack, so a pack without this object is an unfinished upload.

A push lists the `git/*.commits.age` objects, takes the union of their commits as already listed, and
packs the other imported commits with the listed commits as exclusions. Its pack is therefore
incremental: it holds only the objects that the remote does not have. Both objects use
`If-None-Match: *`, as all store writes.

A fetch or clone reads each commits list. For each pack whose commits the local Git store lacks, it
decrypts the pack, checks its SHA-256 against the name, and adds its objects to the local Git store as
loose objects (`git unpack-objects`), as a restored import writes them. A state whose tree is missing
then builds from these objects (`docs/ownership.md`).

Secret checks: a push checks the states it uploads (trees that layr made, and built imports). The Git
objects of unbuilt imports, in a pack, are not checked by that push. They are the project's Git history.

Compatibility: the format stays version 2. The `git/` objects are additive. Older readers ignore them.
A store written before this section has no `git/` objects. A clone of it reads an imported state only
when the state has a tree on the store; otherwise the read fails and names the missing Git pack. The
next push from a machine with the Git objects writes the pack.

## Encoding and signatures

Canonical JSON sorts object keys by Unicode code point. It uses no spaces or fractional numbers.
Strings use Python `ensure_ascii` escapes, including lowercase UTF-16 surrogate pairs.
For example, `{"z":"é","a":1}` becomes `{"a":1,"z":"\u00e9"}`.

A tree is `{"files":{"<path>":["<mode>",size,"<SHA-256>",[links]]},"times":{"<path>":<ns>}}`.
Modes are `100644`, `100755`, and `120000`.
`times` holds the modification time of each file in nanoseconds since the epoch; a restore sets it.
Build tools compare these times (cargo rebuilds a unit whose dependency is newer), so a restored
cache stays fresh. Each key of `times` is a key of `files`. Trees of layr 0.25 and older have no
`times` (the key is left out when it is empty); their files get the time of the restore.
A symlink stores its target bytes as file content.
A link is `["<state UUID>","<attempt>",part,offset,length,"full"|"delta"]`.
Offsets address decrypted data parts. A chain starts with the newest frame and ends with one full frame.
A chain permits at most 16 delta frames. A tree chain permits at most 31 delta trees before a full tree.
A delta uses the previous plaintext as a raw zstd prefix. The compression level is 3.
A file delta requires both versions to be at most 32 MiB and must be smaller than its full frame.
Content with an existing SHA-256 reuses that chain across lines.

The tree header fields are `v`, `state`, `attempt`, `tree`, `base`, `parts`, `machine`, and `sig`.
`v` is 2. `tree` is `full` or `delta`. `base` is null or `["<state UUID>","<attempt>"]`.
The optional `btrfs` field is `{"uuid":"<UUID>","transid":<integer>}`.
An original snapshot uses its UUID and ctransid. A received snapshot uses its received UUID and stransid.
The uploader includes this field only when its UUID equals the signed state's subvolume UUID.
Other snapshot backends omit it. The field is covered by the tree signature.
After file verification, a btrfs restore sets this received identity before it becomes read only and visible.
Without `btrfs`, the restore sets the received UUID from the signed state's subvolume UUID and sets stransid to zero.
Zero identifies a state from another snapshot backend. Its first peer transfer uses a full stream.
`sig` is the standard Base64 Ed25519 signature with padding.
The signed bytes are:

```text
UTF8("layr/store/v2/tree") || 0x00 || canonical(header without sig) || 0x0a || canonical(tree)
```

The machine key comes from its first signed record, as in peer replication.
Records retain their existing signatures over their exact record bytes. Segments do not rewrite records.
New record fields, refs, and attestations travel inside these records without a separate store format.
Live line heads and live refs require their target states and ancestors, including Auto states without `--auto`.
For an imported state the Git pack (section "Git packs") replaces its tree. Trees are required for
states that layr made (named saves, automatic states, merges, and the other kinds that are not imports).
Cache states require `--caches`, even when a live ref targets them. An omitted target is named in the report.
Derived states travel as ordinary states.
Readers validate the contiguous sequence prefix, project identity, signatures, and local trust before they import records.
Readers validate tree signatures and each restored file's size and SHA-256.

State IDs are existing canonical UUIDs. They are not ciphertext hashes.
The design removes RFC HMAC object names, HKDF naming keys, manifests, and CAS refs.
The signed record log carries parents, changes, refs, and attestations.
The immutable tree header authenticates each state representation.

A clone creates working folders only for available line heads. Missing heads appear in `missing_lines`.
The clone retains its project and returns status 2 when some heads are absent.
A later fetch creates the pending working folders when their verified state trees become available.
It preserves existing working folders and their edits.

## Chunks of cache layers

A cache state loses its base at its next save (`history=last`), so a push cannot make a delta
against the old file. Build output also changes paths: cargo names a rebuilt unit with a new hash.
So a cache layer stores its files as content-defined chunks, which match across paths, lines and
machines. Source states keep frames.

- A writer chunks the files of a cache layer when it can open the layer identity. A write-only
  machine writes frames, and a reader reads both.
- Chunks: FastCDC with normalized chunking (`layr_store::chunks`). The minimum is 16 KiB, the target
  is 64 KiB and the maximum is 256 KiB. The gear table and the masks are fixed. A test pins the cut
  points, because a change would stop chunks of older pushes from matching.
- A chunk id is HMAC-SHA256 of the chunk, in lowercase hex. The key is HKDF-SHA256 with the layer
  identity as the input key, the epoch id as the salt and `layr cache chunk key v1` as the label.
  The store cannot test whether it holds a known chunk. A new epoch gives new ids.
- A tree names the chunks of a file in `chunks`: `{"chunks":{"<path>":["<id>",...]},"files":...}`.
  The entry of such a file has no links. Every other file has links and no chunks. The key is left
  out when it is empty, so older trees keep their bytes and signatures. Older readers refuse a tree
  with chunks. The tree header stays `v` 2.
- Packs: `chunks/<pack UUID>.pack.age` holds about 4 MiB of chunks (`PACK_TARGET`; at most one
  chunk more) as one zstd frame at level 12 with long-distance matching and a window up to 2^27.
  `chunks/<pack UUID>.index.age` lists `{"chunks":[["<id>",offset,length],...],"v":1}` in
  plaintext order. Both are encrypted to the layer recipient. The writer puts the pack first and
  the index second. A pack without an index is an unfinished upload, and no tree may name its chunks.
- No object maps chunks to packs. A writer and a reader list `chunks/` and read the indexes, which
  never change after publication. A local copy of each index is kept in `chunk-indexes/`.
- Dedup: a writer skips a chunk that a published index or its own open pack holds. Before it puts
  the tree, it lists the indexes again. A reused chunk that a deletion released meanwhile goes up
  again from the file. The wait of the sweep (below) covers a deletion between that check and the
  tree put. `fsck` reports such a tree, and the next push of its line repairs it.
- Deletion (`chunked::release`): when chunk ids lose their last live tree, each pack that holds
  one is rewritten. Its live chunks go to a new pack and index, and the old pack and index are
  deleted. A pack without live chunks is deleted directly. No dead chunk stays in a pack. The new
  pack is put before the deletes, so a crash leaves a duplicate, and a rerun deletes the same objects.
- Two writers: a sweep releases a dead chunk only when a pass of the same machine found it dead at
  least 2 hours before (`dead-chunks` in the layer store) and finds it dead again. A push of
  another machine that reused the chunk meanwhile names it in its tree, so the chunk is live at the
  next pass and loses its mark. A push that takes longer than the wait can still race.
- A reader that does not find a chunk lists the packs once more: a rewrite may have moved it.
- A restore checks that every chunk of the files it writes is in a published index before it reads
  any file. A cache state that fails is omitted, and the layer store remembers it with a fingerprint
  of the pack names (`cache-restore-failed`). A later fetch tries it again only when a push or a
  deletion changed the packs.
- The remote cache sweep (`store_remote::sweep`) deletes the chunks that its dead trees name and no
  kept tree names, through `chunked::release`, after the trees. A pack that no tree names (a push
  that stopped before its tree) goes when its index is older than a day and holds no live chunk.
  An older key epoch loses all of its `chunks/` with its trees. `layr fsck --caches --remote`
  reports unreachable chunks and kept trees with missing chunks, and exits 1 when either exists.
- `layr_store::reader::chunked_file` reads a chunked file from supplied packs and indexes, also in
  WebAssembly.

- A writer compresses and uploads packs on background threads (one per core), while it chunks
  the next files. It keeps a local copy of the indexes it writes. A restore plans the packs of the
  files it writes, in its order; background threads load and decode them ahead of it, at most 64
  packs ahead (about 256 MiB). A pack outside the plan, or dropped behind it, loads when needed.

- Push manifest: a file whose path, mode, size and modification time (nanoseconds) equal its
  entry in the pushed tree of the nearest pushed ancestor keeps that entry, and the walk does not
  read it, as git and rsync trust their stat data. This works for frames and chunks, also when a
  history limit deleted the ancestor here, because the pushed tree names the times (`times`). A
  reused chunked entry counts as reused for the check before the tree publishes. An entry keeps
  only its own form: a framed entry in a layer that goes up as chunks (a cache pushed before chunks)
  is read again, in a full walk and in a delta, so the cache moves to chunks once, a fetch reads it
  in parallel, and the next sweep deletes the old parts. `LAYR_PUSH_REHASH=1` reads every file again.

Measured with `layr push --caches` to a `file://` store on macOS. Base: a cargo `target/debug`
(2.2 GiB). Small: the same folder after a merge of a few changes (453 files differ). Big: after
every workspace crate was rebuilt (1930 more files differ).

| | frames (0.25.0) | chunks |
| --- | --- | --- |
| first push | 546 MiB, 19 to 20 s | 382 MiB, 22 s |
| next push, small | 37 MiB, 12 s | 20 MiB, 5 s |
| next push, big | 166 MiB, 20 s | 104 MiB, 7 s |
| clone, cold archive (2 runs, alternating) | 19.8, 37.3 s | 18.8, 33.0 s |

The next push reads only changed files (8322 of 8700 files unread for big). For the cold clone,
the archive was on a disk image that was attached again before each clone.

## Keys and recovery

The vault identity is an age X25519 identity. `store.json` contains only its recipient.
A paper key contains the paper identity's 32-byte secret as 24 BIP39 words, as in Desk v1.
A machine slot uses the machine's existing Ed25519 seed through the standard OpenSSH encoding.
The age and ssh-key crates perform the key conversion and encryption.
A machine without a slot can encrypt uploads to the public recipient. It cannot decrypt the store.
Only `remote add` creates a store. A new store requires `--paper-key-out <path>`.
The path must not exist. The caller creates the file with mode 0600, outside local stores, remote stores, and line folders.
No output, record, configuration, or local index contains the words.
Move the words to paper. Delete the key file after you check the paper copy.
S3 credentials come only from the caller's environment. They are not stored in project metadata.

Initialization publishes the paper slot first, the machine slot second, and `store.json` last.
A store without `store.json` is not initialized. No push or fetch creates it.
A retry can use the existing paper file only when it opens the unfinished store's paper slot.
A different paper key cannot complete another writer's initialization.

## Retry and collection

Segments from one machine can overlap. Readers merge them by sequence number.
Duplicate sequence numbers must have identical signed line bytes. A conflict rejects the machine's complete segment set.
Readers import the longest contiguous prefix from record 1. They report a gap after that prefix.
Another segment can fill a gap. An overlap is normal; `fsck --remote` reports its count separately from conflicts.
A writer lists a machine's segments before upload. Its segment starts after the highest last sequence number in that listing.
A record segment keeps its name and signed plaintext across retries.
A 412 means the object already exists. Readers verify its record signatures and reject a conflicting sequence.
A machine identity has one writer. A second store with the same identity (a copy of `machine/`) continues the
records after a clone, but its records differ from the other store's records with the same numbers. Before upload, a
writer that can read the store compares the newest remote segment of its own machine with its local record of that
number. If it already exists, it compares the segment that it was about to write, too. A difference fails the push
("another store uses this machine identity"), and the push publishes no records. `own-records` in the remote's local
folder names the newest own segment that the writer wrote or checked, so that segment needs no read again.
A reader keeps `seen-records` in the same folder (one per overlay layer): each segment that it read and checked, with
its listing time. A segment that the file names with the same time, and whose records are all here, is not read
again, so an idle fetch and the cache sweep of a push read no segment. A replaced object has another time and is
read and checked again. A push that sent no state writes no transfer record, so idle runners make no new segments.
`LAYR_TRACE=1` prints the requests of a fetch, push or clone by kind (`remote get records: 1 request(s)`).
A state retry uses a new attempt. A lost response can leave another complete attempt with the same state ID.
No retry replaces an object. An incomplete attempt does not become visible as a state.
The local index records only attempts whose tree publication completed.

Collection removes only data parts of incomplete attempts older than 24 hours.
Collection checks again for the attempt's tree immediately before deletion.
An upload that reaches 24 hours stops before tree publication. A retry uses a new attempt.
Complete attempts and their delta bases remain.
The store keeps all record segments and complete states by default.

## Errors

| Condition | Result |
|---|---|
| Unsupported store or tree version | Stop before an import or write |
| No readable slot | Reject reads; allow an upload with the public recipient |
| Record sequence gap | Import the contiguous prefix; report the gap |
| Conflicting record bytes | Reject the machine's segment set |
| Invalid record or tree signature | Reject the object |
| Wrong project ID | Reject before importing records |
| Unsafe path, frame range, or excessive chain | Reject the tree |
| File size or SHA-256 mismatch | Reject the restore; do not publish the snapshot |
| Conditional object already exists | Preserve the stored object |
| Uncertain upload response | Return the error; retry with the rules above |
| Malformed S3 multipart completion | Return the error and abort the multipart upload |

## Test vectors

`canonical::tests::equals_python_json_dumps` contains the complete Unicode encoding vector.
`format::tests::signatures_bind_header_tree_and_domain` uses Ed25519 seed `07` repeated 32 times.
The signed tree is `{"files":{}}`. The state and machine UUID are `01a12290-0000-7000-8000-000000000001`.
The attempt is `0123abcd`, the tree type is `full`, the base is null, and the part count is zero.
Without `btrfs`, the signature is:

```text
yIMZ3tHimsJVUe2aJs7RJkVQr9mPaS8QazZnExdCmtVy2i063+jYI5TERQrvxlqYQsxOHTKk0yLlkvPfHfNiCQ==
```

With `btrfs.uuid` equal to that UUID and `btrfs.transid` equal to 7, the signature is:

```text
S7eww50UaRLUiyu646IdgZiC53cdiJQ7gTfMrT7cD9FnDeikWWfB3I4VC6LGDsUi7lSkm3ex6DkYBY7XCL+VDg==
```

Python JSON encoding and OpenSSL Ed25519 independently reproduce both vectors.
Changes to the header, btrfs identity, or tree content must fail verification.
`keys::tests::paper_and_machine_slots_open_the_same_identity` checks both slot forms and a wrong machine key.
`zst::tests::frames_decode_with_and_without_a_base` checks full and raw-prefix delta frames.

Legacy cache layer snapshots do not use this format. The writer reports their names as omitted.
After Core adds cache states, `--caches` carries these ordinary states under `states/<UUID>/`.
Before that, a store push with `--caches` fails with a clear error.

## Overlay objects and gateway access

An overlay uses a separate layer identity for each store remote and epoch.
The epoch is a UUID. Its objects use the v2 record and state formats:

```text
overlays/<overlay UUID>/keys/<epoch>-<record UUID>.json
overlays/<overlay UUID>/epochs/<epoch>/records/<machine>/<first>-<last>.age
overlays/<overlay UUID>/epochs/<epoch>/states/<state>/<attempt>.tree.age
overlays/<overlay UUID>/epochs/<epoch>/states/<state>/<attempt>.<part>.data.age
overlays/<overlay UUID>/epochs/<epoch>/slots/machine-<machine UUID>.age
overlays/<overlay UUID>/epochs/<epoch>/slots/person-<login>-<fp>.age
overlays/<overlay UUID>/epochs/<epoch>/slots/paper.age
```

The key object contains the exact signed `overlay.key` record from the overlay log.
Its data binds `overlay`, `name`, `epoch`, `parent`, `store`, `recipient` and `readers`.
`overlay` is the internal overlay project UUID. `parent` is the parent project UUID.
`store` is the pinned store recipient. `recipient` is the age layer recipient.
`readers` binds machine and person principals to the signed reader set.
The optional `revocation` UUID forces a new epoch after removal, even if a reader is added again before push.
The reader verifies the record against a trusted parent machine key.
The encrypted overlay log must contain the same key record.
Each epoch contains the complete record log and selected states, with no reference to objects in earlier epochs.
The writer publishes slots first, epoch objects next and the public key record last.
Reader removal creates a new epoch. Every object uses conditional create.
Person slot fingerprints use the existing v2 person rule.
The optional `paper_recipient` in the local store pin is public. It contains no secret.

The gateway must apply these access rules:

1. With a write token, permit GET and list only for `overlays/<id>/keys/` objects, which contain public signed key records.
2. With a write token, permit conditional PUT under `overlays/<id>/keys/` and `overlays/<id>/epochs/<epoch>/`.
3. Permit records, states and slots under the epoch for these conditional writes.
4. Require the vault read proof for every other overlay read.

The Layr client implements this contract. The separate `layr-gateway` server must implement the same rules.

### Overlay manager authority

The machine that signs `overlay.create` is the initial key manager.
The creation metadata binds the overlay name and internal project UUID.
`overlay.managers` changes the machine set in the overlay log.
Its data contains `parent`, `overlay`, `name`, `machines`, `previous` and `retained`.
`previous` links to the prior manager record, or is null for the first change.
A current manager must sign the change and remain in its new machine set.

The parent project carries the exact signed manager record in `overlay.authority.data.authority`.
`overlay.authority.data.name` identifies the overlay.
Readers verify the inner signature against a trusted parent machine key.
They apply manager changes through the signed previous-record links, independent of machine clocks.
A missing link or fork stops the authority check.

`retained` maps previously authorized key and readers record IDs to SHA-256 digests of their exact signed line bytes.
It also includes initial overlay access records.
A later manager change preserves all prior retained digests.
This preserves historical records after manager removal and prevents a removed signer from replacing them.
A record from a current manager is authorized. An older record must match its retained digest.
An unauthorized public key object stays unused. The report names its signer.
An unauthorized readers record stops import before it changes the local policy.
Other trusted machines still publish data to the manager's layer recipient.
No additional gateway object path is required.
