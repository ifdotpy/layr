# Storage ownership

This page is part of the [Layr documentation](../README.md#documentation).

Status: implemented. Accepted 2026-11-01; the owners table came first (#115, shadow mode), and
freeing is the default behavior since 2026-11-02. There is no flag and no mode: the old
retention plan is gone.

## Why

Today layr frees disk space like a tracing garbage collector: states pile up, and `layr gc`
walks the whole project to decide what can go. Space grows between runs, a run takes 15 to 60
minutes on the Studio store, and time rules (`undo.days`, `retention.hours`) act only when gc
runs.

The goal is the Rust model: every state has explicit owners, and a state is freed in the
operation that drops its last owner. No walk decides what lives; `gc` only checks and repairs.

## Words

- **Owner**: a fact in the project database that keeps a state. Each owner is one row in the
  table `owners(state, kind, holder)`.
- **Live**: a state with at least one owner. **Dead**: a state with none.
- **Release**: removing an owner row. A release that leaves a state dead frees it.
- **Free**: mark the state dead in the database and move its storage to the trash. The trash is
  emptied in the background (`layr __empty-trash`), as today.

## Owners

| kind | holder | owns | added | released |
|---|---|---|---|---|
| `head` | line id + machine | the line head on that machine | line create, head move | head move, line delete |
| `parent` | child state id | each parent of a live state | the child becomes live | the child is freed |
| `derived` | derived state id | its `derived_from` state | as `parent` | as `parent` |
| `conflict` | state id | `base`, `ours`, `theirs` of each open conflict | the state is made | the state is freed |
| `ref` | ref name | the target | `ref set` | `ref set` to another target, `ref delete`, expiry |
| `tag` | tag name | the target | `tag` | tag delete |
| `remote-ref` | remote ref name | the imported state | fetch | fetch moves it, remote ref delete |
| `export` | remote + branch | the newest exported state of that branch | push | the next push of that branch |
| `park` | line id + index | a park entry | `park` | `park pop`, `park drop`, `park clear` |
| `auto` | line id | its newest `retention.auto` automatic states (default 10) | a new automatic state | the ring overflows, line delete |
| `undo` | line id | the states that its newest `undo.ops` records name (default 20): before and after heads, working states, states made by the record | a record of the line | the ring overflows |
| `deleted` | project | the final state and staged state of the newest `undo.deleted` deleted lines (default 5) | line delete | the ring overflows (with the `undo` owner of the delete record), `undo` of the delete |
| `overlay` | overlay name + line | a project state that a cache overlay head or record names | overlay save | overlay head moves, overlay line delete |
| `store` | store remote name | a state that a successful push to a store remote sent | push | never while eviction is off (the store keeps every state it receives) |
| `pin` | `<pid>:<process start>` | a state that a command reads outside the store lock (a check or `try` copy) | pin start (only when the state has another owner) | pin end; a dead process is released by the next free step and by `gc --repair` |

Rules:

1. Owners are counts, never times. The time rules `undo.days` and `retention.hours` are removed;
   `undo.ops`, `undo.deleted` and `retention.auto` replace them. A ring is a queue: adding entry
   N+1 releases entry 1 in the same transaction.
2. `ref` expiry is the only time rule left. An expired ref is released by the next command that
   reads or writes refs, and by `gc --repair`.
3. A newest automatic state of each live line always has an `auto` owner (ring size at least 1).
4. A deleted line keeps no `undo` owner of its earlier operations. The `undo` owner of its delete
   record lives exactly as long as the matching `deleted` owner `{ord}|{line}`: when the ring drops
   the line, both go, and the final state is freed when no other owner holds it. A ring change
   (`resync_rings`), the replay (`layr reindex`, the first open of a database) and a line delete apply
   the same rule (`owners::undo_allowed`). `undo` of the delete works while both owners exist.
5. Pins are owners that no record makes. The owners check does not compare them.

## When a state is freed

A record that removes the last owner of a state marks the state dead in the same SQLite
transaction that applies the record (`Db::derive`, `owners.rs`): `states.present=0`,
`states.deleted=1`, and a row in `free_queue(state)`. Freeing a state releases its `parent`,
`derived` and `conflict` owners, so a chain that nothing else holds is marked in the same
transaction (a queue, no recursion).

The free step (`free.rs`) runs at the end of every command that opened a project. It takes the
project lock for its check and its move (re-read the queue, skip a state that is owned again, move
the rest to the trash); only the trash emptying runs after the lock. A state that a command names
again before the check is kept (tests/t109-free-recheck.sh). It moves the
storage of each queued state that still has no owner to the trash (`Project::delete_states`),
after the COMMIT and after the store lock is released, then empties the trash in the background.
A crash between the commit and the move leaves the state in `free_queue`. The next command, or
`gc --repair`, finishes it. Moving a state to the trash twice is harmless. `LAYR_TEST_EXIT_AFTER_COMMIT`
(tests only) exits at this point to test the crash case.

A replay (a rebuild of the derived tables, and the owners of `gc` check) runs with freeing off
(`owners::fill_only`). The replay only fills owners.

Before a line delete, its final state gets a complete tree (APFS, `thin::make_keyframe`). The
`deleted` ring keeps that state, and as a thin layer it would keep every automatic state under it.

## Storage that a live state needs

On APFS a thin state is a layer over a base state (`src/thin.rs`). A dead state whose storage is
the base of a live state's layer keeps its storage until no live layer uses it. The free step
asks the layers on disk (`thin::delete_states`), so a dead base stays queued and goes when its last
dependent layer goes, or when a keyframe (the existing upkeep, `layr gc --repair`) re-bases that
layer. The `bases(state, base)` table records each layer's base. btrfs and OverlayFS states have
no layer bases; the table stays empty there.

## States that a store remote holds

A state that a store remote received (a successful push to a store remote) has a `store` owner.
While eviction is off (`gc.evict`, the default) the owner is strong: the state stays with its
history. With `gc.evict on`, `gc --repair` evicts a state whose only owners are the weak kinds
(`parent`, `derived`, `conflict`, `ref`, `tag`, `remote-ref`, `export`, `store`): the tree goes, the
record stays, and a read fetches it back. Git is never a source.

## States imported from Git

An imported commit is a state record without a tree. The `import` record holds the exact Git
objects of the commit, and the state keeps the mapped parents. Git is the source of imported
states: the bare Git repository of the project (`<project>/git`) holds the objects of their
commits and blobs. A read never writes Git.

Reads that need only changes or some files take them from Git and build no tree
(`gitbridge::sparse_pair`, `gitbridge::entry`, `gitbridge::regular_files`):

- `log -p`, `log <path>`, `show <rev>` and `diff` between two imported states (or an imported
  state and its parent). Both sides hold only the paths that differ, filled from git objects, so
  the usual comparison runs on them.
- `show <rev>:<path>`, `blame` and `grep <pattern> <rev>` read the entry, the file or the blobs
  from git. `grep` writes only the ignore files of the commit to a temporary folder.

Commands that need a full tree on disk build it (`Repo::check_state` calls `gitbridge::build_tree`):
`line new`, `try`, `bisect run`, `restore --source` and `reset --hard` to an imported state, and
anything else that opens the tree. The build takes the nearest imported ancestor with a tree on
disk (breadth first, first parent first). The tree is that base plus the changed paths: a thin
layer over the base on APFS, a snapshot of the base with the changed paths elsewhere. Without a
base it is the full tree of the commit. The built state is present; the normal rules apply to it.

- `init --from` and `init --git` build the tree of the commit that the new line starts at.
- `layr git fetch` writes records only. Its commits have no tree.
- Git objects come from the local Git store. A store remote keeps them as Git packs
  (`docs/remote-format.md`, "Git packs"): a push sends the objects of the commits the remote lacks,
  and a fetch or clone installs them. A machine with no Git objects and no pack cannot build an
  imported state. Its record stays, and the read names the missing Git pack.
- When the local Git store is damaged, it is moved aside. The project's git remotes and the
  store remotes that hold packs give the objects back, and then the commits are exported again.
- A copy on a store remote with a tree is the fallback when git cannot give the objects.

Owners and freeing:

- Tree owners keep the tree: `head`, `auto`, `undo`, `deleted`, `park`, `overlay`, `pin`, and
  `conflict` (a conflict needs the trees of its base, ours and theirs). The base of a present
  thin layer keeps its tree too.
- Record owners keep only the record: `parent`, `derived`, `ref`, `tag`, `remote-ref` and
  `export`. Such a state can lose its tree and is built again on the next command that needs it.
- Until the owners model frees states on the operation that drops the last owner (PR #135),
  `layr gc --repair` frees the trees of imported states that no tree owner keeps. It repeats
  until nothing more goes, so a base goes after its layers. `layr gc -n --repair` lists them.
  The record stays.

Costs: a build reads the changed Git trees and writes the changed files. A read from git keeps one
object reader (`cat-file --batch`) for the command and walks the trees of each commit itself, so the
history of a range costs one process, not one per commit. `log` and `show` hold the changed files of
both sides in memory. `diff` between two imported states writes the changed files of both sides to a
temporary folder, which it removes after the diff.

Renames: a read from git shows a moved file as a deletion and an addition, as a built tree does. Git
would show a rename.

## Records from other machines

Peer sync and store fetch import records from other machines. They go through the same
`Db::derive`, so they add and release owners the same way. A record from another machine never
frees a state that a local owner holds: owners are per state, not per machine.

## Pins

A command that reads a saved state while the store lock may be free pins it: a row `pin` with holder
`<pid>:<process start>`. `free::pin` takes the pins of a set of states (`Repo::pin`), and
`Repo::hold` keeps them until the command ends (`free::finish`). A pin counts only for a state that
has another owner. A state that no owner names, or that a free step already queued, is refused:
the reader fails with "state X was freed: no line, ref, tag or undo record keeps it", and it never
reads the content of a freed state. A pinned state that loses its other owners stays until the pin
ends; the end of the command marks it dead, and the free step of that command frees it. Pins of dead
processes are released by the next free step and by `gc --repair`. A pinned state also keeps its
ancestors, because its parent rows stay.

A reader of an UNOWNED state (an old leftover) is not pinned, by design: pinning it would count as an
owner and keep it. Only an explicit `gc --repair` racing such a read can remove it.

Covered (each call site pins its states for the whole command):
- save and every write command (`prepare_save`: the head and the save base), save with caches;
- merge (the source), merge queue entries (the source and the expected head), pick and revert (the
  picked states), replay (the head, the upstream and the onto state), undo (every state the record
  names);
- `line new` (the start state and the source head, with its caches);
- status and the other overlap compares (`overlaps`: every compared head, read before the pins);
- diff, show and log (the revisions they name and the line head);
- `try` and checks (the state under test).

Test: tests/t100-pin-reads.sh. The test hook `LAYR_TEST_PIN_HOLD_MS` (test only) sleeps once per
process after its pins are taken, so a second process can drop the last owner of a state that the
reader holds.

## Temporary objects

A crash must not leave space that nothing owns. Every temporary path that layr makes on the
store's disk has an owner too: the process that made it. Before a command creates a temporary
path it adds a row `temps(path, pid, start, kind)` (one SQLite statement, outside the store lock
is fine); when the path is renamed into place or removed, the row goes. `gc --repair` and the
start of every command remove the paths of rows whose process is gone (pid not alive, or a
different process start time).

This covers, by kind:

| kind | paths |
|---|---|
| `copy` | `.apfs-<uuid>` beside a destination, `meta/work/*`, `meta/scan/*`, prepared saves |
| `receive` | `states/.receive-<uuid>` and its stream spool |
| `snapshot` | a state or cache snapshot made before its record: it becomes owned by the record in the transaction that commits it; a crash before the commit leaves a dead temp row |
| `line` | a line folder published before its `line.create` record, a line folder and stage after its `line.delete` record (the delete moves them to the trash as a temp, then finishes) |
| `try` | `lines/.try-*` (`--keep` turns the row into a normal owner) |
| `file` | `.<name>.tmp-*` of `atomic_write` and `cache_write`, OCI layer archives, `.layr-dedup-*` |
| `remote` | `.put-*` of a `file://` store remote (the remote has its own `temps` file next to its objects) |
| `git` | temporary git index and fetch folders: they move from `$TMPDIR` to `meta/tmp/git` |

Rules: no temporary path is created outside a folder that `gc --repair` walks; a failure path
removes its temporary object at once (a guard), and the `temps` row is only for crashes.

Disk caches outside the project database (the mount cache of `layr mount`, `meta/oci/blobs`)
count their partial files (`.part-*`) in their size budget and remove partial files older than
one hour at open.

## Derived data of a state

Some data exists only because a state exists. It has the state as its owner and goes when the
state is freed:

- the git index `git/layr-index/<state>` and the internal git ref `refs/layr/states/<state>`;
  after a free, layr runs `git gc --prune=now` in the background when more than 1 GiB of loose
  or unreferenced objects collected (`git count-objects`);
- OCI derived refs: a `ref` owner with an expiry (`oci.retention`, default 30 days), not `None`.

Quarantine data (`git-damaged-*`, `*.layr-old-*`) is never freed automatically: it can hold the
only copy. `layr gc` lists it with its size and age, and `layr gc --drop-quarantine <path>`
removes one after the owner looked at it.

## Trash

The background emptier (`layr __empty-trash`) stops when a full pass deletes nothing, and writes
the paths it could not delete with the error to `trash.errors`; `layr gc` and `layr du --store`
show them.

## gc

`layr gc` is a check. It recomputes the owners from the records, compares them with the table
(pins excluded) and lists the present states that no owner names (leaks). It prints one line with
the leaks and their bytes, and exits 1 when anything differs.

`layr gc --repair` replaces the table with the replay (live pins kept), marks the leaked states
dead, releases expired refs, trims cache snapshots beyond `retention.caches`, runs the existing
cache and overlay upkeep, runs the free step, and empties the trash. `layr gc -n --repair` only
prints what it would free. `gc --dedup` is unchanged. `layr fsck` runs the same check, and
`fsck --repair` rebuilds the table.

The imported-state free step (section "States imported from Git") is in `gc --repair` before
PR #135. It uses the owners table in shadow mode and does not depend on #135.

The check also reports stale `undo` owners: owners of a deleted line whose delete record is outside
the `deleted` ring (a store written before rule 4, or a leftover). It prints their count and the bytes
of the states that the repair frees. `layr gc -n --repair` lists the states the repair would free.

## Configuration

| key | default | unit | meaning |
|---|---|---|---|
| `retention.auto` | 10 | states per line | automatic states a line keeps |
| `undo.ops` | 20 | records per line | operations of a line that `layr undo` can reverse |
| `undo.deleted` | 5 | lines per project | deleted lines that `layr undo` can bring back |

`undo.days` and `retention.hours` are removed. Setting either fails with the name of its
replacement. Ring sizes are counts. The first open of an old database does not free anything:
the replay fills the owners, and `layr gc` reports the states that no owner names.

## Upgrade

- A database that already has the owners table (since #115) keeps working as it is. Freeing starts
  with the next operation that drops an owner.
- A database from before #115 is rebuilt from the records with freeing off (`owners::fill_only`).
  States that the replay leaves unowned stay on disk.
- `layr gc` prints how many states and bytes `layr gc --repair` would free. The first repair
  frees them. No command frees them all at open.

## Tests

- tests/t100-pin-reads.sh: a merge source, a save base and an overlap compare stay on disk while
  their reader runs, and go after it.
- tests/t99-ownership-free.sh: line delete frees its states in that command, the `undo.deleted`
  ring, the auto ring, `undo.ops`, the crash between commit and move, a pinned read, the time keys,
  `gc` check and repair.
- tests/apfs-ownership-bases.sh (APFS only): a dead base of a live thin layer keeps its storage and
  goes with the layer.
- tests/t92-owners-shadow.sh: the owners table equals the replay after each step.
- tests/t120-deleted-line-owners.sh: a deleted line's final state is freed when the `undo.deleted` ring
  drops it; a ring change gives a deleted line no earlier undo; undo inside the ring works; gc reports
  and repairs the stale undo owners of a store made before the rule.
