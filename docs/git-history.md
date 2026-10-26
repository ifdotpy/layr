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

The writer keeps a content hash index for the push and compares each snapshot
with an available parent. Unchanged entries reuse the parent's authenticated
file links. Record segments contain at most 4 MiB of signed JSONL, except for a
single larger record. Segments publish in sequence so a retry can continue after
the last complete segment.

The local `pushed.json` cache contains attempt and depth metadata. Separate files
hold complete cached trees, loaded one at a time. Metadata checkpoints occur
every 128 states and at successful completion. An older complete-tree index is
migrated when opened. After interruption, committed remote trees recover states
that were uploaded after the last checkpoint. Cache files and metadata do not
change the encrypted store format.

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
