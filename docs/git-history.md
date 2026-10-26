# Git history across stores

`layr git clone`, `layr init --git`, and `layr git fetch` import every commit
reachable from each selected branch. Each imported commit has an `import` state
with the same ordered parents and file tree. Merges remain merges. Repeating an
import reuses completed states.

An import record contains the original commit ID and base64-encoded raw commit
object, including author, committer, timezones, signatures, encoding headers and
message bytes. It also contains raw tree objects absent from its first parent's
tree. Existing signed records and age encryption carry this metadata through
store push, fetch and clone. File content uses the existing state transfer path.
The local bare Git repository is a cache of these objects.

On another machine, the bridge restores the tree objects, recreates missing blobs
from state files, and checks their Git hashes before writing the original commit
object. Exporting an imported state therefore preserves its commit ID. New layr
states produce new commits whose parents are the original mapped Git IDs.
Tracked ignored files, executable modes, symlinks and Git link entries retain
their original tree representation.

`layr git push <remote> <line>` refuses a non-fast-forward update and reports both
the existing remote ID and the proposed ID. Fetch and merge the remote history,
or pass `--force` to replace it deliberately. Git also checks the update when the
remote branch changes while the push is running.

Imports created before this behavior do not contain the raw object metadata.
Fetching their original Git branch again imports the complete reachable DAG as
new states. Existing immutable states remain available. A store made only by an
older binary cannot recover original commit bytes after its Git cache is lost.

Run `sudo tests/run.sh t49-git-history` on a supported test filesystem to check a
five-commit merge DAG through an encrypted store and fresh clone, exact object
IDs, a new child commit, repeated import, and refused/forced rewinds.

## Transfer cost and recovery

The writer keeps a content hash index for the current push, seeds it from needed
parent trees, and compares each snapshot
with an available parent. Unchanged entries reuse the parent's authenticated
file links. Record segments contain at most 4 MiB of signed JSONL, except for a
single larger record. Segments publish in sequence so a retry can continue after
the last complete segment.

Secret checks build one current private source index per push, then inspect each
state's changes against its first parent. A root state has no parent, so its
complete tree is checked. The index is rebuilt on the next command.

The local push cache uses compressed, content-addressed directory nodes in
`push-cache.sqlite3`. Unchanged subtrees share their SHA-256 node hash across
states. The database page limit grows from 512 KiB to 1/32 of the compressed state
payload uploaded by this writer, with a hard ceiling of 16 MiB. A persisted byte
counter survives eviction. When the limit is reached, the writer evicts its nodes
and caches the current tree again. A tree larger than the limit is
left uncached. This database uses an in-memory rollback journal and can be
removed or rebuilt after interruption or corruption.

`pushed-v2.json.zst` holds compressed attempt, depth and root-hash metadata for
at most 16,384 completed states. Metadata checkpoints occur every 128 uploads
and at successful completion. Older metadata and per-state JSON trees migrate
on the next push; obsolete tree files are removed before uploading so migration
can free disk space. Missing metadata is recovered from exact remote state
prefixes, and missing parent trees are hydrated from authenticated store objects.
A cache failure never changes whether a remote tree was committed. Hydration
recovers the exact authenticated delta-chain depth, so eviction preserves both
content links and the cadence of full tree checkpoints. After a
crash or disk-full failure, already published immutable objects are reused.

While a push is active, `push-journal.sqlite3` keeps its content hash index and
published-state cursor in a separate FULL-synchronous SQLite transaction per
state. The cursor advances only after the state tree publishes. A restart reads
authenticated trees only for publications beyond that cursor. Pending part
entries are recorded before upload; recovery decrypts each part and verifies its
file hashes through the normal reader before reusing it. Missing or corrupt
parts are discarded from the journal. Old unreferenced parts are eligible for
the store's 24-hour garbage collection grace period; file-backend `.put-*`
temporaries are private publication files and are excluded from object listings.

The transfer journal does not evict hashes during a push. It warns above 64 MiB
and is removed after successful completion. It is separate from the bounded,
rebuildable tree cache. `LAYR_TRACE=1` reports its peak disk size, commit time,
and restart seeding time and reader GET count.

Completed metadata avoids listing old remote states on each push; new states
use their own prefixes. Advertised line and ref tips are rechecked with their
exact state prefixes, so a later push can repair a partial replica that lacks a
published tip tree. Cache files and metadata do not change the encrypted store
format or the original Git commit IDs.

Fetch restores parents before children. It clones an available immutable parent
snapshot and writes only files whose mode, size or content hash changed. Decoded
tree caching is bounded to two delta chains, and a verified cached ancestor stops
repeat decoding of its prefix. `LAYR_TRACE=1` reports transfer phase timings.

Loose objects in the rebuildable Git cache publish with atomic renames, using
Git's normal cache durability model. The signed records and state files remain
the durable source. Rebuilding Git objects does not force a filesystem flush for
every object.
The bridge hashes commit and current-tree objects when reading them. Git pushes
check every reachable object before packing it. A full cache scan runs only after
a read or hash check fails. If validation fails, it moves the damaged cache to a private `git-damaged-*`
folder and rebuilds the requested history from signed records and state files.
The retained folder can contain fetched refs that were not imported yet.
