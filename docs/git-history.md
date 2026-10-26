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
