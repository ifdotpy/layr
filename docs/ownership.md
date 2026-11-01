# Storage ownership

This page is part of the [Layr documentation](../README.md#documentation).

Status: proposed 2026-11-01. Implementation follows the phases at the end.

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
| `deleted` | project | the final state and staged state of the newest `undo.deleted` deleted lines (default 5) | line delete | the ring overflows, `undo` of the delete |
| `overlay` | overlay name + line | a project state that a cache overlay head or record names | overlay save | overlay head moves, overlay line delete |
| `pin` | process id | a state a running command reads (a prepared copy, a merge source) | command start | command end; a dead process is released by repair |

Rules:

1. Owners are counts, never times. The time rules `undo.days` and `retention.hours` are removed;
   `undo.ops`, `undo.deleted` and `retention.auto` replace them. A ring is a queue: adding entry
   N+1 releases entry 1 in the same transaction.
2. `ref` expiry is the only time rule left. An expired ref is released by the next command that
   reads or writes refs, and by `gc --repair`.
3. A newest automatic state of each live line always has an `auto` owner (ring size at least 1).

## When a state is freed

A record that removes the last owner of a state frees it in the same SQLite transaction that
applies the record (`Db::derive`). Freeing a state releases its `parent`, `derived` and
`conflict` owners, so a chain that nothing else holds is freed in the same transaction, in one
pass from the child to its ancestors (a queue, no recursion).

The transaction only marks states dead (`states.present = 0`, `states.dead = 1`) and adds them
to `free_queue(state)`. After the commit and after the store lock, the command moves each
queued state's storage to the trash and removes it from the queue. A crash between the commit and
the move leaves the state in `free_queue`; the next command or `gc --repair` finishes it. Moving
to the trash twice is harmless.

## Storage that a live state needs

On APFS a thin state is a layer over a base state (`src/thin.rs`). A dead state whose storage is
the base of a live state's layer keeps its storage until no live layer uses it. Track this as a
separate storage edge `bases(state, base)`, written when a layer is made:

- Freeing a state with no live dependent layer moves its storage to the trash.
- Freeing a state that is still a base keeps its storage and marks it `dead`. When the last
  dependent layer is freed or re-based (keyframe, the existing upkeep), the base storage goes.
- btrfs and OverlayFS states have no layer bases; the table stays empty there.

## States that a store remote holds

A state that a layr store remote holds (a push record names it; the store never deletes complete
states) keeps its record when its local storage goes. Its storage has a weaker kind of owner:
only `head`, `auto`, `undo`, `deleted`, `park`, `overlay`, `pin` and `bases` keep the local
copy. `parent`, `derived`, `ref`, `tag`, `remote-ref` and `export` keep the record but not the
copy. Reading such a state fetches it back (`Repo::check_state`, as in #111). Git is never a
source.

## Records from other machines

Peer sync and store fetch import records from other machines. They go through the same
`Db::derive`, so they add and release owners the same way. A record from another machine never
frees a state that a local owner holds: owners are per state, not per machine.

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

`layr gc` becomes a check and repair:

- `layr gc` (check): recompute owners from the records into a temporary table, compare with
  `owners`, and print the difference: leaks (dead by the recomputation but live in `owners`) and
  missing owners (live by the recomputation but dead or missing). Exit 1 when they differ.
- `layr gc --repair`: replace `owners` with the recomputation, finish `free_queue`, empty the
  trash, release pins of dead processes and expired refs.
- `layr fsck` runs the check.

## Configuration

| key | default | unit | meaning |
|---|---|---|---|
| `retention.auto` | 10 | states per line | automatic states a line keeps |
| `undo.ops` | 20 | records per line | operations of a line that `layr undo` can reverse |
| `undo.deleted` | 5 | lines per project | deleted lines that `layr undo` can bring back |

## Phases

1. **Owners table, shadow mode.** Add `owners`, `free_queue`, `bases`; fill them from the records
   (`layr reindex` and on open of an old database); keep them current in `Db::derive`. Nothing is
   freed by them yet. `layr gc` prints the difference between its current plan and the owners
   model and still uses its current plan. Test: on every test store the two agree, or each
   difference is explained in this page.
2. **Release on operation.** Behind `storage.ownership = on` (project config, default off):
   records free states as described, `gc` becomes check and repair. Test: a line delete frees its
   unique states in that command; a ring overflow frees the oldest automatic state; `undo` within
   the rings works; a crash test (kill between commit and trash move) leaves no leak after the
   next command.
3. **Default on.** Remove the time settings and the old plan. Document the change in
   docs/commands.md.

Each phase is one PR and works on APFS, btrfs and OverlayFS.
