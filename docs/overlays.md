# Overlays

Status: file overlays support shared and follow mode (section 9), store encryption and peer sync (section 10). Sections 1
to 8 are the design and the experiments of 2026-10-11; section 9 says what the slice does, where
it differs from the design, and what is left. Section 1 holds the model of the owner decision
2026-11-11 (lines and overlays are independent; a view joins them); the rest of the file
describes the stack model that the code builds today.
[Cache overlays](cache-overlays.md) adds overlay properties, folder overlays and remotes per layer.

An overlay is a file tree with an owner, readers and properties. A view shows a line across
several overlays as one folder (section 1, owner decision 2026-11-11). With overlays, a project keeps files in history that not every reader
of the project may see: local settings such as `.env`, keys, personal notes, private data.
Today such files are either ignored (no history) or in history for every reader.

Short result:

- A named save can make the new state of each changed layer from its previous state plus its part
  of the change list. This took 10 to 13 ms per layer, for overlays of 10 to 10,000 files.
- Making the states from the full tree instead (snapshot and delete, empty subvolume and
  reflinks) grows with the overlay: about 27 ms for 10 files, 107 ms for 1,000 and 301 ms for
  10,000.
- Assembling a line from the shared state and the overlay files: 20, 48 and 144 ms.
- Sorting a change list by owner is cheap: under 1 ms for a state of 54 files.
- A folder overlay as a nested subvolume costs 7 to 8 ms at any size. But `rename()` across
  its edge fails with EXDEV, `git mv` fails, and git can delete the subvolume.

## 1. Model

Owner decision 2026-11-11. The model below replaces the stack model of this file. The code builds
it in slices (section 1a); "Built today" after the list says how the code differs.

- A **line** has its own history. Lines form a tree, as branches in git. A line does not know
  about overlays.
- An **overlay** is a layer of files. Overlays stack, like sheets of glass: a view looks through
  all of them at once. An overlay has an owner, readers and properties
  (`folder`, `quiet`, `history`, `merge`, `platform`, id keying; see
  [cache overlays](cache-overlays.md)). An overlay does not own lines. Lines and overlays are
  independent entities.
- The **base** (the project itself, called the shared set below) is also an overlay: the bottom
  overlay. Its readers are everyone who reads the project. It is the only overlay that
  `layr git push` sends to the git remote.
- A line runs across the overlays. One state of a line holds its part on each overlay. A
  line has the same name on every overlay.
- A **view** is a function of a line and a set of overlays. Only the view joins a line and a
  set of overlays. For the same path the upper overlay wins, as in overlayfs. The same path (for
  example `.env`) can be on many overlays.
- `fetch` takes lines and overlays: the history of the chosen lines, only their parts on the
  chosen overlays. Example: a merge runner fetches the line `main` with the base and the `target`
  overlay.
- A cache is an ordinary overlay ([cache overlays](cache-overlays.md) section 2).
- **New files go to the base.** A file goes to an overlay by an explicit command
  (`layr overlay move`) or by a routing pattern of the overlay. The patterns are in the
  overlay's metadata in the project database, not in a file of the tree, so a change of the
  tree cannot change where a file goes.
- **A merge is atomic across the overlays** that the merge takes. The user who merges must be a
  reader of all of them. The checks run on the full result, with all overlays in place.
- **Encryption.** In remote copies and in remote vaults, an overlay can be stored encrypted for its
  readers with age.

Built today (section 9):

- A line is the same line on every overlay: the store of each overlay holds it with the same id
  and the same name (`main` on the base and `main` on `target.macos-aarch64`). Its head there is
  the drawing of the line on that overlay. Built 2026-11-12; see "Lines on overlays as built"
  below. Overlays of the old shared mode keep one line per user (`user-<uid>`).
- The view of the working folder (setting `view.<line id>`) chooses the overlays that the folder
  shows and their order; the head of the line on each overlay gives the state. The setting
  `stack.<line id>` is gone: layr reads it only for a line of layr before 0.44 that
  `overlay migrate-parts` did not move yet.
- A state record of the project names the shared tree and its part on each public overlay
  (`parts`), build output included. A hidden overlay records the part in its own records
  (`hidden.part.<state>`), so the public record never names it.
- `fetch` takes the whole repository by default, except the contents of the cache overlays: of a cache
  overlay a plain fetch, pull or clone takes only the records, and a command that needs a cache fetches it
  on demand (owner decision 2026-11-13, [remotes](remotes.md)). `fetch --line <name> --overlay <layer>` (built
  2026-11-11) takes the history of those lines on those layers only; `base` names the base. The old
  cache flags of transfers (`--caches`, `--no-caches`, `--caches-of`) are removed (exit 2).
- Since slice 3 an overlay can have `platform=<key>`; since slice 6 a cache is a folder overlay of one
  platform (`target.macos-aarch64`), and a named save records its part in the state. The cache
  code (`src/cache_overlay.rs`) still makes and reads those parts, and the parts are still full
  folder snapshots, not v2 objects (slice 7).

The modes as built:

  | Mode      | The overlay line of a line                              |
  | --------- | ------------------------------------------------------- |
  | follow    | gets a new line with the source line and merges with it               |
  | shared    | one overlay line for all lines of the user              |
  | ephemeral | not built: gets a new line with the source line, and is deleted when it merges |

In this file a **layer** is the shared set or an overlay of a stack (an overlay of a view in the new
model).

## 1a. The model in code (owner decision 2026-11-11, target)

This section turns section 1 into data and commands. Each part below is one slice; section 1
"Built today" says what the code does until the slice lands.

**Records.**

- A state of a line names its part on each overlay: `layers: {<overlay>: <part id>}` in the state
  record. The base part is the state itself (its tree and times table). An overlay with no part in a
  state is clear for it: the view shows the overlays below.
- A part lives in the store of its overlay (`<project>%<overlay>`), encrypted for the readers of the
  overlay, with the ids of that overlay (plain or keyed, docs/objects.md section 2). Its delta base
  is the part of the first parent state.
- History belongs to the line: parents, merges and undo are the parents of the states. The store of
  an overlay holds the same line, with the same id and name, so a line has the same name on every
  overlay because there is one line. Its head on an overlay can be ahead of the part that its head
  state names: a protected line moves only through merges, so a change of only an overlay there
  moves the line on that overlay and makes no state of the line.
- Properties of an overlay: `folder`, `quiet`, `history`, `merge`, `platform`, readers, and
  `trust` (a part that another user saved is taken only from a protected line; today
  `cache_by` and `cache_shared`).

**Platforms.** An overlay with `platform=<key>` is shown and fetched only on that platform, and a plain
fetch takes only its records (its states come with `fetch --overlay <name>`, `--line`, or on demand when a
new line takes a cache). Build
output of two platforms is two overlays (`target.macos-aarch64`, `target.linux-x86_64`); a
platform key is no part of a line name.

**Views.** A view is f(line, overlays): an ordered list of overlays over one state of a line. The
working folder of a line is its default view: every overlay that the caller reads and whose
platform is this machine's, in the project order of overlays. `layr view <line> --overlay a,b`
makes another view. There is no `stack.<line id>` (built 2026-11-12).

**Commands.**

| Command | Effect |
| --- | --- |
| `save -m` | Saves the changed parts of every overlay of the view. A quiet overlay (a cache) is saved with it. |
| `merge <line>` | Merges per overlay with the overlay's `merge` property, as one operation. |
| `line new X <line>` | X starts at the state of `<line>` with all its parts. |
| `fetch <remote> --line L --overlay G` | The states of L, and only their parts on G (built, slice 1). |
| `push <remote> --line L --overlay G` | The same selection for a push. |
| `gc` | A part outside `history` of its overlay is dropped; the state keeps its record. |

`save --caches`, `sync --caches`, `merge --caches-only`, `fetch --caches/--no-caches/--caches-of`,
`fsck --caches` and the `cache.` name prefix go. Caches are stored as v2 objects like any part.

**Overlay-only saves (owner decision 2026-11-11, built).** A line has its own history: a save
that changes only overlays that keep parts makes a state of the line with the base tree of its
parent and the new parts. In git that state maps to its parent's commit, so git gets no empty
commit. A protected line moves only through merges, so there such a save keeps the part in the
stack as before. Test t233 section 7.

**Shared mode (owner decision 2026-11-11, built).** An overlay has a part per line, and a new
line inherits the part of its source, so one personal file (`.env`) needs no shared mode: put it
on an overlay whose readers are its owner. `overlay create --mode shared` fails with exit status 2;
the default mode is follow. Overlays made in shared mode before keep working.

**Hidden overlays (owner decision 2026-11-11, built partly).** A state of a line records its part
on each hidden overlay in that overlay's own records (`hidden.part.<state>`), which a remote copy
encrypts for the overlay's readers; the public state record never names a hidden overlay. A view of an
older state (`layr view <state>`, `layers_at`) shows that part. The head of a line still reads the
overlay's overlay line. Test t238.

**Local space and history of parts (owner decision 2026-11-11, built).** A part of a build-output overlay
gives up its native copy once its objects are durable; the cache code reads it through
`state_root`, which builds the copy again from the objects. `history=last` also drops parts that
are here only as objects, and `layr gc` prunes each object store (the base and every overlay): the
records of states that no kept state needs, and the loose objects and hot blobs that kept trees,
times tables, delta bases and chunk lists do not reach. Packed objects stay until a pack rewrite.
`replay` keeps the parts of each state it replays; a mount of an older state shows its parts; a
merge brings an overlay that the incoming line shows into the view of the target line. Tests t236,
t58, t23, t37; unit test `prune_keeps_what_kept_states_reach_and_drops_the_rest`.

**Slices.**

1. `fetch --line --overlay` (built).
2. `push --line --overlay` (built).
3. Overlays per platform; no platform key in names. Built 2026-11-11; see "Slice 3 as built"
   below for what differs.
4. State records name their parts; save, line new, merge and view use them; overlay
   stores keep parts only. Migration of existing stores. Built 2026-11-11 for public follow
   overlays; see "Slice 4 as built" below for what differs.
5. Views hold the overlays; `stack.<line id>` goes. Built 2026-11-11; see "Slice 5 as built"
   below for what differs.
6. A cache is an ordinary overlay (default view shows cache overlays like any overlay; transfer cache flags removed): the cache flags, `src/cache_overlay.rs` and the cache fields of
   the state record go; `trust` is an overlay property. Built 2026-11-11 in part; see "Slice 6 as
   built" below for what differs.
7. (migrate-parts also gives line heads their parts on folder overlays and sets the platform of an old `cache.<folder>` overlay whose states have one platform; t237.) Parts of folder overlays as v2 objects (built, partly): in a project with `objects.enabled`, a
   part of an overlay goes to the overlay's own object store (`meta/overlays/<overlay>/meta/objects`),
   written incrementally against the part before it (`objstore::enabled`, `record_states`). The
   immutable flag of sealed files is handled by the store's clone hook. Left: the native copy of a
   part stays (the overlay code reads parts as folders), so the space gain is in pushes and in
   dedup, not yet on the local disk; history=last drops the part record, and its objects wait for
   a repack that walks reachability. Test t236.
8. Layr Cloud: a state with parts needs no API change (checked 2026-11-12 against layr-cloud
   `crates/layr-gateway`). Each overlay is already its own layer scope (`overlays/<name>/`) with
   its own `PUT /states/<state>` of one tree and times table, so a part is a state of its overlay's
   scope. The signed records carry `parts` and `state.parts` as opaque lines: the gateway
   validates only manifest and job-output records, and `StateRec` in its vendored `layr-model`
   accepts unknown fields.

**Lines on overlays as built (2026-11-12).** This replaces what the notes of slices 4 to 6 below
say about overlay lines, the stack and the `cache` flag.

- Names. The line of `main` on an overlay is `main`, with the id of `main`. A command finds it by
  name, then by id (a line that took a new name), then by a name of layr before 0.44
  (`line-<line id>.<os>-<arch>`, then `line-<line id>`), and gives it the name of the line at its
  next write. `line rename` renames it on every overlay of the machine. `line new`, a save, a
  merge and a folder that takes a cache make it when it is missing. A line that is gone loses its
  lines on the overlays (`gc`, `fsck --overlays --repair`), on every overlay, not only on caches.
- Layers. The layers of a line folder are the overlays of its view, each at the head of the line
  on that overlay, else at the part that the head state names, else at the empty first state. An
  older state shows the parts that it names. `reset --hard`, `overlay use`, a view in a new folder
  and `undo` of a save move the line on the overlays; a record of the project names no stack.
  A save record names the parts it made (`made: [{overlay, state}]`) for `undo`.
- No `cache` flag. A build-output overlay is an ordinary overlay with properties. Each rule that
  read the flag reads a property now:

  | Rule | Property |
  | --- | --- |
  | The folder takes part only where the line marks it `regenerable` | `platform` |
  | `overlay remove` takes it; a store remote deletes its states of older key epochs | `platform` |
  | Its readers follow the trusted machines; the paper key need not open it | `trust` |
  | A fetch takes only the states that its records keep; a push deletes the others on the remote; `fsck --overlays` counts the others | `history` |
  | A store remote keeps its files in chunks | `folder` |

  A fetch of an overlay that `--overlay` did not name reports a failure of that overlay and goes on.
- Migration. `layr overlay migrate-parts` also moves each line of layr before 0.44: its old stack
  becomes the view of its folder and the heads of its lines on the overlays, its overlay lines take
  its name, and the `cache` flag of each overlay goes (`trust` becomes `protected` where it was
  unset). Until then a line with no view shows its old stack, and the old names are found.
- Tests: t233 (lines with the line's name), t234 (an old stack and its migration), t59, t110, t144.

**Slice 4 as built (2026-11-11).**

- Records. A state record has `parts: {<overlay>: <part id>}`. The key is `parts`, not `layers`:
  `layers` is already the JSON key of the cache snapshots of a state (`caches`), so an older
  layr would read a `layers` map of parts as caches. A state without parts writes no key, so
  older records and older clients see no change. A part id is the id of a state in the store of
  the overlay (`<project>%<overlay>`).
- Which overlays. Only a public follow overlay that is no cache keeps parts (`parts_overlay` in
  `src/overlay.rs`). Shared overlays (`user-<uid>`), hidden overlays and cache overlays keep their
  overlay lines as before. A hidden overlay is not in `parts`, because a public state record would
  show its name to every reader of the project.
- `save -m`. A save that makes a new shared state names the part of each such overlay of the stack
  in it, the changed parts and the unchanged ones. A part gets its next state from the part that
  the line showed, so its delta base is the part of the first parent. A part keeps that part as
  its parent: the three-way merge of an overlay and `undo` need it. The overlay store gets a record
  with the new part and no overlay line. A line that already has an old `line-<line id>` keeps
  moving it, so the old data stays true.
- A save that changes only overlays makes no shared state (as before): the stack of the line
  (`stack.<line id>`) holds the new part until the next save of the shared set. So the stack
  stays the source of the current parts of a line until slice 5.
- `line new X <rev>`. When `<rev>` is the head of a line, X takes the stack of that line. When it
  is an older state with parts, X takes those parts. A state without parts gives the stack of the
  source line, as before. A state without a line gives the overlays of its parts.
- `reset --hard <state>` shows the parts of the state. An overlay of the stack that the state does
  not name is clear (its empty first state). The other modes keep the stack. `undo` of such a
  reset is refused, as for other operations that changed overlays.
- `merge <line>` combines each overlay with its `merge` property, as before, and the merge state
  names the merged parts. `merge <state>` (no line) takes the parts of that state on the overlays
  that the target shows. A queued merge takes no overlays, as before.
- `view <state>` shows the state with its parts. `view <line>` and `view save` use the stack.
- `overlay log|show|diff|grep|ls-files <overlay>` read from the part that the current line shows
  when the overlay has no overlay line for the line.
- `fetch --line L` and `push --line L` take the parts that the states of L name, the part of the
  stack of L on each machine, and the history of each part.
- `gc` with `history=<n>` keeps the parts of the newest n states of each line and the parts that
  the stacks show.
- Migration: `layr overlay migrate-parts [-n] [-q]` (a project admin). It gives the head of each
  line its parts from the line's stack (the heads of its overlay lines), with one `state.parts`
  record. The signed save records stay as they are. A state that the head of two lines shows
  takes the parts of the line that saved it, else of the first line by name. An overlay that a state
  already names keeps its part. `layr migrate` does not run it.
- History before the migration: older states keep no parts. Their overlay lines stay, and every
  command reads them as before (`line new`, `reset` and `merge` of such a state use the stack of
  the line). No old data is deleted.

**Slice 5 as built (2026-11-11).**

- Storage. The view of a working folder is the setting `view.<line id>`: the names of its public
  overlays, bottom first. A line has one working folder on each machine, so the key is the line id;
  the view of a folder on another machine is `view.<line id>@<machine>`, as for stacks. Records
  set it with `folder_view: {line, overlays}` (`line.create`, `overlay.use`, `line.adopt`, and
  `overlay remove`). The key is not `view`: the records of saved views (`view.save`) use that key.
  Hidden overlays keep their place in their own store (`hidden.view.<line id>`; layr before 0.44
  wrote `hidden.stack.<line id>`), so the public setting never names them. The line record names no overlay.
- What stayed until 2026-11-12. `stack.<line id>` held the state that the folder showed on each
  overlay of its view. Since 2026-11-12 the head of the line on each overlay holds it (see "Lines on
  overlays as built").
- Compatibility. A line without `view.<line id>` (made before slice 5) shows its stack as before.
  Its first `overlay use` gives it a view. Older layr ignores `folder_view` and reads the stack,
  which every record still writes.
- Default view. `line new` (and `init`) gives the new folder every overlay that the owner reads, in
  the project order: the names of the setting `overlays.order` (`layr overlay order a,b,c`), then
  the other overlays in the order of their creation (the time of their empty first state), then by
  name. The order refuses hidden names (it is public). Left out: folder overlays (no overlay of the
  file stack), an overlay whose `platform` setting is not this machine's (no command sets `platform`
  until slice 3), an overlay that is not on this machine, and a cache overlay that the source line does
  not show (until slice 6 makes caches ordinary overlays). The part on each overlay: the part that the
  start state names, or the source's state when the start is the source's head; else the empty
  first state.
- `overlay use <names>` changes the view of the current folder. An overlay that comes back shows the
  part that the head names.
- `save -m` and `merge` act on the overlays of the view. The new state also keeps the parts of the
  head on the overlays that the view does not show, so a folder that shows fewer overlays does not
  clear them.
- `view <line or state> --overlay a,b` prints another view of the same line or state with those
  overlays, in that order (`base` is always below). `--folder <dir>` writes the files of a view to
  a new folder: a second folder of one line with other overlays. It is a copy, not a working folder.
- Test: tests/t234-views-hold-overlays.sh.

**Slice 3 as built (2026-11-11).**

- Property. `layr overlay create|set <overlay> --platform <key>` sets `overlay.<overlay>.platform`
  (`<os>-<arch>`, for example `macos-aarch64`; `--platform any` clears it). `overlay list` shows
  `platform=<key>`. The key has no hash of key files: the key parts of a build
  ([cache overlays](cache-overlays.md) section 6) still decide which part of the overlay fits a line.
- Which commands. The default view of a new line folder leaves out an overlay of another platform.
  Every folder command (`line new`, `save`, `merge`, `status`) uses only the folder overlays of this
  platform: an overlay of another platform has no folder here. `push` and `fetch` to a store remote
  and to Layr Cloud leave out an overlay of another platform and say so (`overlay far: an overlay of
  platform linux-x86_64`), unless `--overlay` names the overlay. Peer sync (`layr sync`) does not
  filter: the peer may be of either platform.
- Names. A new build-output overlay is `<folder>.<platform>` (`/` as `.`), and its overlay lines are
  `line-<line id>`, with no platform key. One folder can have one overlay per platform:
  `overlay create --folder` refuses a second overlay of the same folder and platform, and an overlay
  without a platform clashes with every platform.
- Old data. A `cache.<folder>` overlay (no `platform`) stays the overlay of its folder on every
  platform, with its overlay lines `line-<line id>.<os>-<arch>`. When an overlay of this platform
  exists for the same folder, it wins. Test: tests/t235-cache-is-an-overlay.sh.

**Slice 6 as built (2026-11-11).**

- `save -m`. A named save saves the part of each build-output overlay of the line: each folder that
  `regenerable` (`.layrattributes` or `layr config regenerable`) marks and that is a layer. A
  folder that has no part on the line, or in which a file or folder changed since the part (by
  modification time, in nanoseconds), gets a new part; an unchanged folder keeps its part, so a
  named save does not copy big build output each time. The new state names the part in `parts`.
  The first named save makes the overlay of this platform, with `folder`, `quiet`, `history=last`,
  `merge=union-theirs`, `platform` and `trust=protected`. A failure is a warning; the save goes on.
- `line new X <state>` takes the part that the state (or its nearest ancestor) names, before the
  older rules (`cache_of`, the overlay line of the line that made the state). `line new X <line>`
  takes the line's part, as before.
- `trust`. `layr overlay create|set <overlay> --trust protected|readers`. `protected`: a line takes a
  part that another user saved only from a protected line. `readers`: any part that the user
  reads. A folder overlay without the property is protected. The state fields `cache_by` and
  `cache_shared` stay: they record who saved a part and whether the line was protected, and the
  rule reads them. They have the JSON keys `layer_by` and `layer_shared`.
- Old flags. `--caches`, `--no-caches` and `--caches-of` on `push`, `fetch`, `pull`, `sync` and
  `clone` are removed: each fails with exit status 2 and names `--line` or `--overlay` (owner
  decision 2026-11-11). Every overlay comes by default. `save --caches`, `merge --caches-only` and
  `fsck --caches` are `save --overlays`, `merge --overlays-only` and `fsck --overlays` now; the old
  spellings fail with exit status 2. The follow pass and the check save-back call `save --overlays`.
- `cache.` prefix. A new overlay has no `cache.` prefix. A store keeps its `cache.<folder>` overlays
  and the old cache fields of its state records; every command reads them as before.
- Migration of a store: nothing to run. An existing `cache.target` stays the overlay of `target`:
  each named save records its part in the state from then on. To give a store one overlay per
  platform, an admin makes the new overlay on each platform and removes the old one when every
  line has a part on the new overlay: `layr overlay remove cache.target` (cache-overlays.md section
  18), then the next named save of each line makes `target.<platform>`. The parts of the old
  overlay go with it; the next build fills the new overlay.
- What is left of the slice: `src/cache_overlay.rs` keeps the build-output code (overlay creation,
  platform key, part choice, prune, old data); only the general folder overlay code moved to
  `src/overlay.rs`. The `cache` setting (`overlay.<overlay>.cache`) is gone since 2026-11-12; see
  "Lines on overlays as built". Parts are still full folder
  snapshots (slice 7).

**Open (owner decision needed).**

- Shared mode (`user-<uid>`: one overlay line for all lines of a user) does not fit the model, in
  which an overlay holds a part of each state of a line. Slice 4 keeps it as it is for old and new
  data. The owner must decide: drop the mode, or keep it as an overlay without history per line.
- Hidden overlays keep their overlay lines (see above). They need a place for their parts that
  does not show their names, for example a record in the hidden store keyed by the state.
- Slice 6: should `overlay remove cache.<folder>` happen by itself (for example in `layr migrate`)
  once each line has a part on the new overlay of its platform? Today the admin runs it.
- Slice 6: the cost of a named save with changed build output is one folder snapshot (on APFS a
  copy of every file, made before the store lock). Is that acceptable for every `save -m`, or
  should a project choose that only `save -m` on protected lines saves the part?
- A save that changes only overlays makes no shared state, so the state record and the stack can
  differ until the next shared save. The model says a state holds its parts; the alternative is a
  new shared state with the same tree for each such save (an empty commit in git).

## 2. Commands

| Command                                         | Effect                                                         |
| ----------------------------------------------- | -------------------------------------------------------------- |
| `layr overlay create <name> --readers <principals>` | Makes an overlay. The caller is its owner. Principals are as in `layr access`: a user, `@group`, `*`. |
| `layr overlay route <name> <patterns>`          | New files whose path matches a pattern go to the overlay.      |
| `layr overlay move <name> <paths>`              | Moves files of the line to the overlay. The next named save removes them from the layer that had them. |
| `layr overlay use <names...>`                   | Sets the stack of the current line, from the bottom to the top, above the shared set. |

The other commands keep their meaning on the line as a whole: `status`, `diff`, `add`,
`save -m <message>`, `log`, `show`, `merge`. Section 7 lists what is not decided yet, for example the
place where a mode is set. Section 9.1 has the commands as built (`route` is not built).

## 3. How it maps to btrfs

### 3.1 The line stays one subvolume

There is no overlayfs. A line is one writable subvolume that holds the files of all its
layers, so every program sees a normal folder. Each layer keeps its own states (read-only
subvolumes). The head of a line becomes a set: one state of the shared set and one state per
overlay of the stack.

Next to the head, the line keeps a read-only snapshot of its full tree: the **base**. The line
is a writable snapshot of the base, so `status` and `diff` still compare two snapshots of one
tree with `btrfs send --no-data`, in O(changes), as today.

### 3.2 Named save

1. The change list of the state comes from `btrfs send --no-data`, as today.
2. Each changed file gets an owner: the highest overlay of the stack whose head state has the
   path; else the highest overlay with a routing pattern that matches the path; else the shared
   set. Folders belong to no layer: a layer has a folder when it has a file in it.
3. Each layer with changes gets a new state.
4. One record names the new states and the new base.

There are two ways to make the new state of a layer (step 3):

- **Split** (from the full tree). The shared state is a writable snapshot of the line with the
  overlay files deleted, then made read-only. An overlay state is an empty subvolume plus
  reflink copies of the overlay files, then made read-only. The cost grows with the number of
  overlay files.
- **Apply** (from the previous state). A writable snapshot of the layer's previous state, plus
  the layer's part of the change list (reflink copies, deletes, moves), then made read-only.
  The cost is one snapshot per changed layer plus the changed files.

At 10,000 overlay files, split takes 125 ms for the shared state and 176 ms for the overlay
state; apply takes 10 to 13 ms per layer at every size (section 5). So a named save uses apply.
Split is for the first state of an overlay (`layr overlay move` of many files) and for a repair.

### 3.3 Assembling a line

A new line from a line with the same stack (follow mode, same readers) is a snapshot of the
source line's base: O(1), as `layr line new` is today.

A line needs assembly when its stack differs from the source: a line of a user who reads other
overlays, or a line after `layr overlay use`. Assembly is a writable snapshot of the shared
state, then reflink copies of the files of each overlay, from the bottom to the top, so that
the upper file wins. A read-only snapshot of the result becomes the base. The cost grows with
the number of overlay files: 20 ms for 10 files, 48 ms for 1,000 and 144 ms for 10,000.

### 3.4 Folder overlays

An overlay that holds one whole folder can be a nested subvolume at that folder, as a cache is.
A snapshot of the line leaves a nested subvolume out: the snapshot has an empty folder there.
So:

- The shared state is a read-only snapshot of the line. Nothing is deleted.
- The overlay state is a read-only snapshot of the nested subvolume.
- Assembly is a snapshot of the shared state, `rmdir` of the empty folder, and a snapshot of
  the overlay state at that path.
- Each layer has its own `btrfs send --no-data` change list. No file needs an owner lookup.

This took 7 to 8 ms for split or assembly, for a folder of 185 files and for one of 8,117
files. The cost is at the edge of the folder: section 6.

### 3.5 Push, remote copies and replication

- `layr git push` exports only the states of the shared set. A state that changed only overlays
  makes no shared state, so git gets no commit for it.
- Store pushes include every overlay. Each overlay has a separate age key for each store remote.
- Peer sync includes each overlay that permits the peer machine. Section 10 defines these rules.

## 4. Who sees what

- A user sees the files of an overlay only as a reader of it. `layr overlay use` needs that.
- `show`, `diff`, `grep`, `blame`, `log -p`, `try` and `export` of an overlay state need the
  read right of the overlay, in addition to the role in the project.
- The base and the automatic states of a line hold the files of all its layers. Section 7,
  question 5.

## 5. Measurements

### 5.1 Method

- Machine: an OrbStack Linux VM on an Apple M5 Max, 18 vCPU, 15 GiB memory. Linux 7.0.14
  (OrbStack build), btrfs-progs 6.17.1, GNU coreutils 9.7, Python 3.14.4, git 2.53.0.
- File system: btrfs on an 8 GiB loop file, made with the mkfs.btrfs defaults (16 KiB nodes,
  no-holes, free-space-tree) and mounted with `ssd,discard=async,space_cache=v2`. The loop
  file is on the btrfs disk of the VM.
- Tree: a copy of the working folder of a real application (a desktop program, a Python
  server, a web client, built packages), without version control folders and installed
  packages: 10,824 files in 1,032 folders, 609 MB.
- Overlays: 10, 1,000 and 10,000 files, picked at random over all files with a fixed seed.
  10,000 files are 92% of the tree.
- Every timed step ends with `btrfs filesystem sync`, so it includes the btrfs transaction
  commit. The file system is synced before each run. The times include the start of the
  `btrfs`, `xargs`, `cp` and `rm` processes (0.3 to 0.5 ms each); layr calls the ioctls
  directly. Reflink copies use `cp -a --reflink=always --parents`.
- 5 runs of each step. The tables give the median and the range (minimum to maximum) in
  milliseconds. In a second full run, the medians of the totals were within 7% for steps above
  40 ms, and within 40% for the short steps.
- The page cache was warm, and the file system was small and quiet. On a large file system
  under write load, one snapshot takes 15 to 600 ms (README, Performance).

The scripts are in `docs/overlays-bench/` (appendix).

### 5.2 Reference

| Step                                                     | Median | Range        |
| -------------------------------------------------------- | -----: | ------------ |
| one read-only snapshot of the line (a state today)      | 6.0    | 4.3 to 7.5   |
| four read-only snapshots (one per layer of a stack of 4) | 18.8   | 17.2 to 25.0 |

### 5.3 Named save, split method (from the full tree)

| Step                                         | 10 files            | 1,000 files          | 10,000 files           |
| -------------------------------------------- | ------------------- | -------------------- | ---------------------- |
| shared: writable snapshot of the line        | 4.7                 | 12.7                 | 4.5                    |
| shared: delete the overlay files             | 1.3                 | 21.9                 | 78.5                   |
| shared: make read-only, sync                 | 13.4                | 37.9                 | 39.1                   |
| **shared state, total**                      | **19.6** (17.4 to 21.2) | **69.4** (63.5 to 116.0) | **124.7** (106.8 to 128.6) |
| overlay: create an empty subvolume           | 0.4                 | 0.5                  | 0.5                    |
| overlay: reflink copies of its files         | 1.2                 | 21.4                 | 139.0                  |
| overlay: make read-only, sync                | 6.1                 | 18.0                 | 27.2                   |
| **overlay state, total**                     | **7.8** (6.3 to 8.8) | **37.1** (34.4 to 41.9) | **176.0** (166.6 to 221.3) |

Per file at 10,000 files: about 8 µs to delete and 14 µs for a reflink copy.

### 5.4 Named save, apply method (from the previous state)

A writable snapshot of the overlay's previous state, reflink copies of 10 changed files,
read-only, sync.

| Overlay size | 10 files            | 1,000 files         | 10,000 files       |
| ------------ | ------------------- | ------------------- | ------------------ |
| total        | 12.5 (10.5 to 13.8) | 10.7 (8.3 to 15.8)  | 9.5 (7.8 to 10.6)  |

### 5.5 Assembling a line

| Step                                          | 10 files            | 1,000 files         | 10,000 files           |
| --------------------------------------------- | ------------------- | ------------------- | ---------------------- |
| writable snapshot of the shared state         | 4.9                 | 3.8                 | 3.4                    |
| reflink copies of the overlay files, sync     | 13.5                | 41.9                | 140.9                  |
| **total**                                     | **19.5** (17.1 to 21.7) | **48.0** (40.8 to 54.5) | **144.3** (134.0 to 145.9) |

A read-only snapshot of the result as the base adds one snapshot (5.2).

### 5.6 Folder overlay as a nested subvolume

| Step                                                                    | 185 files          | 8,117 files        |
| ----------------------------------------------------------------------- | ------------------ | ------------------ |
| split: read-only snapshots of the line and of the folder, sync          | 7.4 (7.3 to 10.0)  | 6.8 (6.7 to 8.3)   |
| assembly: snapshot, `rmdir` of the placeholder, snapshot into it, sync   | 8.3 (8.2 to 8.7)   | 7.8 (7.1 to 8.4)   |

### 5.7 Owners of a change list

Two states, made in a writable snapshot of the line. The typical one: 40 changed files, 5 new
files (2 of them in `secrets/`, which a routing pattern matches), 5 deleted and 2 moved: 54
files and 64 paths with the folders. The large one: 1,000 changed files and the same others:
1,014 files and 1,025 paths. The stack has three overlays (10, 1,000 and 10,000 files), each
with five routing patterns. The owner rule is the one of 3.2: an `lstat` in the head state of
each overlay from the top, then the patterns.

| Step                                                    | typical (54 files) | large (1,014 files) |
| ------------------------------------------------------- | ------------------ | ------------------- |
| `btrfs send --no-data -p` (layr does this today)        | 5.4 (5.3 to 6.3)   | 28.8 (27.4 to 33.0) |
| read the paths from `btrfs receive --dump` (Python)     | 0.6 (0.6 to 0.7)   | 10.4 (10.2 to 10.5) |
| find the owner of each file (Python)                    | 0.3 (0.2 to 0.9)   | 4.0 (3.9 to 7.8)    |

Owners in the typical state: shared set 9, top overlay 3, middle 1, bottom 41. The Python
times are an upper limit for the same work in Rust. layr already parses the stream in Rust.

## 6. Folder overlays and common programs

The test (`tools.sh`) makes a line with a nested subvolume `sec` and 1 MiB files, so that no
file is inline. "Reflink" means that the copy and the original start at the same physical
extent (`filefrag -v`).

| Action at the edge of the folder overlay                                     | Result                                                        |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `rename()` of a file from the line into the folder, and back                 | EXDEV (Invalid cross-device link)                             |
| `link()` into the folder                                                     | EXDEV                                                         |
| `rename()` of the folder itself inside the line                              | works                                                         |
| reflink (`FICLONE`, `cp --reflink=always`) across the edge                   | works, extents shared                                         |
| `cp` with default options (GNU 9.7, uutils 0.10)                             | works, reflink                                                |
| `mv` of a file or a folder, GNU coreutils 9.7                                | works: copy with reflink, then delete                         |
| `mv` of a file, uutils coreutils 0.10                                        | works, but copies all data (no shared extents)                |
| Python 3.14 `shutil.move`, `shutil.copy2`                                    | works, reflink                                                |
| save in the folder: temporary file in the folder, `fsync`, `rename()`        | works; `sed -i` and `vim` (`:wq`) work, the folder stays a subvolume |
| save with the temporary file outside the folder                              | EXDEV                                                         |
| `git status`, `add`, `commit`, `checkout` of branches that change files in the folder, `stash` | work, the folder stays a subvolume          |
| `git mv a.txt sec/`                                                          | fails: `fatal: renaming 'a.txt' failed: Invalid cross-device link`, exit 128 |
| `git checkout` of a commit without the folder, and `git rm -r sec`           | deletes the subvolume; a later checkout makes a plain folder  |
| the folder made a subvolume again (reflink copy of its files)                | `git status` stays clean                                      |
| `rm -rf sec` by a normal user who owns it, mount without `user_subvol_rm_allowed` | deletes the subvolume (`rmdir` of an empty subvolume)    |
| `btrfs subvolume delete sec` by that user                                    | refused: Operation not permitted                              |
| `find -xdev`                                                                 | stops at the folder (another `st_dev`): 3 of 13 files         |
| read-only snapshot of the line                                               | the folder is an empty placeholder (inode 2), not in `btrfs send`; `rmdir` removes it in a writable snapshot |

## 7. Open questions

1. **Delete of a path that a lower layer also has.** A user deletes `.env`, which is in an
   overlay and in the shared set. The state deletes it from the overlay, and the next assembly
   shows the shared `.env` again. overlayfs keeps a whiteout for this. Does an overlay need a
   delete marker, or does the delete go to every layer that the user may write?
2. **Owner of a moved file.** A file of an overlay moves to a path that no pattern matches. Does
   it stay in its overlay (the owner follows the file; btrfs reports moves by inode) or go to
   the shared set (the owner follows the path)? Following the file is safer: a move never
   publishes a private file.
3. **Ignore rules.** `.env` is usually ignored, and layr does not save ignored files. A path
   that a routing pattern sends to an overlay must be tracked there. Do routing patterns win over
   ignore rules, and do ignore rules apply per layer?
4. **The identity of a reviewed result.** `merge --expect`, approvals and check results name one
   state today. With overlays a head is a set of states. A proposal: a stack record (the shared
   state and the overlay states) with its own id, which `--expect`, approvals and checks name.
5. **Readers of assembled states.** The base and the automatic states (`save`, before every
   changing command, every 10 minutes) hold the files of all layers. Today the read right of
   the project covers every state. Such states must be readable only by the readers of all their
   layers, or automatic states must be split by layer too (one more snapshot per layer).
6. **Review by a user who does not read an overlay.** Does `diff main...topic` hide the overlay
   paths, show only their names, or refuse? The merge rule already needs a reader of all follow
   overlays.
7. **Shared mode and two lines.** With one overlay line for all lines of a user, a state in
   line A moves the overlay line, and line B still has the old files. The next state of B must
   not undo A's change without notice: refuse when the overlay head moved, or merge it in?
8. **Where a mode is set.** The owner sets the default and a line overrides it. Is that an
   option of `overlay create` and `overlay use`, or a separate command?
9. **A lost folder overlay.** `git checkout`, `git rm -r` or `rm -rf` deletes the nested
   subvolume (`rm -rf` was also tested as a normal user), and git can make a plain folder
   there. The next state
   would then put the folder's private files into the shared set. layr must refuse a named save when
   the path of a folder overlay is not a subvolume, and offer a repair, as `layr overlay repair`
   does for caches.
10. **Moves across the edge of a folder overlay.** `rename()` gives EXDEV: `git mv` fails, GNU
    `mv` copies with reflinks, uutils `mv` copies all data. A file overlay has no such edge. Are
    folder overlays worth it, or only for large private folders where O(1) assembly matters?
11. **Programs that stay on one file system.** `find -xdev` stops at a nested subvolume. Remote copy
    and copy programs with a one-file-system option probably skip folder overlays too (not
    tested).
12. **A file that was pushed.** `layr overlay move` of a file that is in a pushed shared state
    does not remove it from git: the old commits keep it. Should the command warn, or refuse
    without a flag?
13. **age keys.** Where do the public keys of readers live (records in the project database?).
    A reader who is removed can still read old encrypted remote copies.
14. **Replication.** Readers are users of a machine, as roles are. Which overlays does
    `layr sync` send to a peer: none, all encrypted, or only to machines with the same readers?
15. **Checks and private files.** A check runs on the full result as the caller, who reads all
    follow overlays. A check that copies its tree to another place can copy private files there.

## 8. What to build first

Steps 1 and 2 are built for file overlays, with assembly (step 3) and shared mode (step 4) as
the two real uses need them (section 9). Routing patterns, `blame` and `try` with overlays, the
stack record of question 4, ephemeral mode, encryption and folder overlays are not built.

1. **File overlays in follow mode, with the apply method for named saves.** Records: an overlay (owner,
   readers, patterns), the stack of a line, the stack head. Commands: `overlay create`, `route`,
   `move`, `use`. Reader checks on `show`, `diff`, `grep`, `blame`, `log`, `try`, `export`.
   Push of the shared set only, with a test that no overlay file reaches git. This covers the
   common case (`.env`, keys) and keeps a state at about one snapshot per changed layer.
   Decide questions 1 to 5 before this step.
2. **Merge across follow overlays**, with the stack record for `--expect`, approvals and checks.
3. **Assembly** for lines with another stack (a line of another user, `overlay use`).
4. **Ephemeral mode**, then **shared mode** after question 7 is decided.
5. **Encrypted remote copies** with age.
6. **Folder overlays** last, and only for large private folders. They need the subvolume check
   of question 9.

Not measured: a cold page cache (the VM was shared, so caches were not dropped); a large file
system under write load; a Rust version of the owner lookup and of apply; apply with many
changed files (at about 14 µs per reflink copy, 1,000 files add about 14 ms); age encryption;
merges with overlays; stacks of more than three overlays.

## Appendix: scripts

All scripts are in `docs/overlays-bench/` and run as root on Linux with btrfs:

| Script        | What it does                                                                          |
| ------------- | ------------------------------------------------------------------------------------- |
| `setup.sh`    | Makes the loop file system, copies the tree into the line, picks the overlay files.  |
| `bench.sh`    | Times sections 5.2 to 5.7 and prints one TSV row per step and run.                   |
| `classify.py` | Reads a `btrfs receive --dump` listing and finds the owner of each file (5.7).        |
| `summary.py`  | Prints the median and the range of each step from the TSV rows.                      |
| `tools.sh`    | Runs the tests of section 6, some of them as a normal user.                          |

```sh
W=/root/ovl-bench docs/overlays-bench/setup.sh <source folder>
docs/overlays-bench/bench.sh 5 <folder> <folder> >bench.tsv   # folders for 5.6
docs/overlays-bench/summary.py <bench.tsv
docs/overlays-bench/tools.sh <uid of a normal user>
umount /root/ovl-bench/mnt && rm -rf /root/ovl-bench          # clean up
```

The two folder overlays of 5.6 were folders of the tree with 185 and 8,117 files.

## 9. Built: the first slice

Built on 2026-10-11: file overlays (any paths of the tree) in shared and follow mode. The tests
are in `tests/t16-overlays.sh`; the model is in `src/overlay.rs`.

### 9.1 Commands

| Command                                                     | Effect                                                                                    |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `layr overlay create <name> [--readers <principals>]` | Makes an overlay (an overlay). The caller is its owner and reads it. Readers are principals as in `layr access`: `alice`, `@group`, `uid:1001`, a comma list. Each line has its part; a new line inherits the part of its source. `--mode shared` was removed (owner decision 2026-11-11, exit 2); old shared overlays keep working. Needs the writer role. |
| `layr overlay list [--json] [--size]`                        | The overlays of the project: mode, owner, readers. `*` marks those the current line shows. |
| `layr overlay readers <name> [<principals>...]`             | Without principals: the readers. With them: sets the readers (the owner only).             |
| `layr overlay use [<name>...]`                              | Without names: the stack of the current line. With names: the stack, bottom first; each overlay takes its newest state for this line. `--add` and `--remove` change one overlay and keep the others; `--none` clears the stack. The owner of the line only, who must read each overlay. The overlay files come into the folder or leave it; a local change in the way refuses (`-f` overwrites). |
| `layr overlay move [--purge] <name> <paths>...`             | The files (a folder: all files below it) go to the overlay at the next named save, which takes them from the working folder as they are then. The move waits for that named save whatever happens to the index; `--cancel [<paths>]` drops it. The line shows the overlay first if it did not. `--purge` deletes the automatic states of the line that hold the files. |
| `layr overlay log\|show\|diff\|grep\|ls-files\|blame <name> [<args>]` | The command on the overlay's own history, as a reader: at the overlay line of the current line, else the caller's, else the owner's. `layr --project <project>%<name>` names an overlay too; it takes only reading commands. |
| `layr overlay show <name> --json` | Metadata for a reader: name, hidden flag, readers with principal and kind, key manager machine UUIDs, signed epochs, patterns, peers, and current state ID. Person principals omit their age recipient. |
| `layr overlay log <name> --ops [--json]` | All operations from the overlay's own signed records, newest first. JSON is an array with the CLI operation name, actor, time and record ID. Payloads and key material are omitted. |

The metadata JSON fields are `name`, `hidden`, `access_mode`, `readers`, `managers`, `epochs`, `patterns`,
`peers`, `state`, and `properties` (`history`, `quiet`, `merge`, `gc`; see [cache overlays](cache-overlays.md)). Readers use canonical principals and kinds `user`, `group`, `everyone`,
`machine`, or `person`. Each epoch has `number`, `id`, `time` in milliseconds and `machine`.
Numbers start at 1 in signed key record application order (time, machine, sequence), rather
than UUID order. A later signed record for the same epoch ID gets its own number. `reason`
appears only if the signed record contains it. Routing patterns are not built, so `patterns`
defaults to `[]`. `peers` is the configured machine array, or null for the default peer policy.
`state` uses the same overlay line selection as the history commands and falls back to the
empty root state when there is no overlay line. Hidden settings come from the overlay's
own records. A non-reader gets the same bytes and exit code as for an overlay that never
existed. The ordinary human history output stays unchanged.

The other commands work on the line as a whole, as in section 2: `status`, `diff`, `add`, `rm`,
`mv`, `restore`, `reset`, `save -m <message>`, `merge`, `log` of the line's shared history.

### 9.2 Storage and records

- An overlay is the internal project `<project>%<overlay>` in `projects/<project>/meta/overlays/<overlay>/`
  (inside the private `meta` folder): its own states, signed records and access policy. Its owner
  is its admin; its readers have the reader role. Owning a line of an overlay does not give the
  read right, unlike in a project.
- Its lines have no working folders: `user-<uid>` in shared mode (one per user) and
  `line-<line id>` in follow mode (one per line). The first state is empty. Since slice 4
  (section 1a) a public follow overlay that is no cache gets no new `line-<line id>`. Owner decision
  2026-11-11: a line has the same name on every overlay, so these names go (section 1).
- The project records `overlay.<name>` (owner, mode, first state) and, for each line,
  `stack.<line id>`: the overlays the line shows and the state of each. A record that changes a
  stack carries `stack` in its data; `reindex` rebuilds the stacks from the records. Owner
  decision 2026-11-11: a line holds no stack; a view joins a line and overlays (section 1).
- The base of a line with overlays is `meta/base/<line id>@<key>`: a read-only snapshot of the
  head with the overlay files in place, the key a hash of the head and the stack. It is derived:
  made again when missing. `fsck` compares it with a new assembly.
- No record of the project holds a path of an overlay. The paths of `overlay move` wait in
  `meta/stage/<line id>.moves` until the named save or `--cancel`.

### 9.3 Named save

1. The saved tree is the index (with `-a`, the changed files too; staged into a copy, so a
   refused save leaves the index as it was).
2. The change list from the base to that tree is split by layer. A change goes to the highest
   layer that has the path. A new path goes to the shared set, unless `overlay move` named it. A
   moved file (by inode, or by content as git finds renames) stays in the layer of its old path.
   A delete goes to every layer that has the path.
3. Each changed layer gets its next state from its previous one: a writable snapshot, its
   changes as reflink copies, read-only (the apply method of section 3.2).
4. The records of the overlays first, then one record of the project with the new shared state
   (if the shared set changed) and the new stack. The saved tree becomes the base.

A protected line takes a state that changes only its overlays from its owner: the rule protects
the shared set. A change of the shared set still needs the rule (`--direct`), as before. In the
same way, the checks, approvals and path rules of a protected line apply to the shared set of a
merge; its follow overlays change with the merge.

Who writes an overlay: a reader changes it only in its own overlay lines. In shared mode that is
the reader's `user-<uid>` line; in follow mode it is the line's own overlay line, which the line
owner changes with named saves and a user who merges into the line changes by the merge.

### 9.4 Who sees what

- Overlay content is read only through the overlay's project, so its policy decides: `overlay log`,
  `show`, `diff`, `grep`, `ls-files` and `--project <p>%<o>` refuse a user who does not read it, and
  such a user cannot list its paths.
- The working content of a line with overlays (`status`, `diff`, `ls-files`, `grep`, `blame`,
  `show :<path>`) is open to its owner and to users who read all of its overlays.
- A new line from a line shows the overlays of the source that its owner reads; other overlays
  are left out, so the folder of a non-reader never gets their files. A line made from a state
  shows none, unless the state names parts (section 1a, slice 4): then it shows those parts.
- Automatic states of a line with overlays leave the overlay files out, the paths of pending
  moves, and copies of overlay files under other names (moved, or the same content). So no state
  of the project holds an overlay file, and `push`, `export`, `sync` and `show`/`grep`
  of a shared state give the shared set only. Push and sync also carry the separate overlay projects.
- Automatic states made before a file moved into an overlay hold it (an automatic state holds the
  whole working folder, ignored files too), and every reader of the project reads them. `overlay
  move --purge` deletes those states of the line. States of the shared set that held a file keep
  it: layr does not rewrite history, and a pushed state stays on the remote (`overlay move`
  says so).

### 9.5 Merges

- Shared mode: an overlay is not part of a line's history. A merge changes the shared set only;
  the overlay files of the target stay in place.
- Follow mode: a merge of a line merges its follow overlays into those of the target, in the same
  operation: a fast-forward or a three-way merge in the overlay (conflicts as in the shared set).
  The user who merges must read each of them, and the target must show them; otherwise the merge
  is refused. A merge that changes only the overlays makes no shared state.
- `pick`, `invert` and `replay` take the shared set only. `merge <state>` (not a line
  name) merges the shared set only, unless the state names parts (section 1a, slice 4): then
  it also merges those parts on the overlays that the target shows.

### 9.6 Answers to the open questions (for this slice)

1. A delete goes to every layer that has the path.
2. A moved file stays in its layer (the owner follows the file).
3. `overlay move` wins over ignore rules: an ignored file can move into an overlay. Routing
   patterns are not built.
5. Automatic states leave the overlay files out (one more step per save in a line with overlays:
   a writable snapshot, the deletes, read-only).
7. Shared mode: a state that changes an overlay is refused when the overlay line moved since the
   line took its state (the user saved a state in another line). `overlay use <names>` takes the
   newest state, then the named save succeeds.
8. The mode is an option of `overlay create`. A line cannot override it.
12. `overlay move` of a file of the shared set notes that earlier states keep it, and the remote
    too when the project pushed.

Not decided by the slice: 4 (the stack record), 6 (`diff main...topic` for non-readers shows the
shared set only, which is safe but not a decision), 9 to 11 (folder overlays), 13 to 15.

### 9.7 Limits and what is left

- Not built: routing patterns (`overlay route`), ephemeral mode, folder overlays, `--expect` of a stack
  (question 4: `--expect` names the shared state; the follow overlays are merged as they are).
- `save --amend` and `park` refuse in a line with overlays.
- `line delete` and `line delete -D` of a line with unsaved changes of overlay files (or a pending move)
  need `-f`: automatic states do not keep those changes.
- `overlay use` and merges into a line with overlays never overwrite an ignored file of the user
  (for example a key of its own at the path of an overlay file): they refuse, and `-f` overwrites.
- A conflict of a merge of a follow overlay is listed by `status` and stays listed while the file
  has markers; `restore --ours` and `--theirs` do not cover it.
- `rm` or `rm --cached` of an overlay file takes it out of the overlay at the next named save; a file
  left in the folder is then an ordinary untracked file.
- `undo` of a state moves the overlay lines back and keeps the changes in the folder. `undo` of
  a merge that changed follow overlays, and of `overlay use`, refuse.
- Checks of protected lines (`try`, merge checks) run on the shared set only, without overlay
  files.
- In shared mode each reader has an own overlay line. A new reader starts from the state of the
  source line (or the owner's); a later change by the owner does not reach other users'
  lines. A key that changes is changed in each reader's lines.
- Overlay files have no automatic states: their history consists of named states. Automatic states also
  leave out a file that moved from an overlay path and a new file with the same content as an
  overlay file (a copy, an editor's extra copy); a changed copy under another name (a swap file) goes
  in.
- `export` of the working content (no revision) and `try` give the shared set only.
- If the owner of a line stops reading an overlay of its stack, the line refuses named saves until
  `overlay use --remove` takes the overlay out; its files stay in the folder as untracked files.

### 9.8 Measured

`docs/overlays-bench/save.sh`: a release build in an OrbStack Linux VM, btrfs on a loop file,
a tree of 10,000 small files, one line with the overlay and one without for each overlay size,
5 runs each, `btrfs filesystem sync` before each state. A state of 10 changed files, 5 in the
shared set and 5 in the overlay:

| Overlay size | without overlays (median, ms) | with the overlay (median, ms) |
| ------------ | ----------------------------: | ----------------------------: |
| 10 files     | 51                            | 99 (54 to 114)                |
| 1,000 files  | 32                            | 66 (64 to 67)                 |

Each changed layer state took 7 to 12 ms (a writable snapshot, 5 reflink copies, read-only), as
section 5.4 measured. Other steps with the 1,000 file overlay: `status` of a clean line 16 ms
(8 without), `save` 34 ms (15 without), a new line from the line with the overlay 44 ms (20
without; the assembly copies the overlay files). The debug build in `tests/t16-overlays.sh`
prints its own numbers.

### 9.9 Example: a private loader and a project key

The owner of `main` in project SAP has `.codex-keychain.envrc` (a local loader for that user only)
and `.zulip.env` (a project key, ignored by `.gitignore`, for the users of the group `sapdev`) in
the folder of `main`. The shared `.envrc` loads the key with `dotenv_if_exists ./.zulip.env`. In
the folder of `main`:

```sh
layr overlay create keychain                     # readers: the owner only
layr overlay move --purge keychain .codex-keychain.envrc
layr save -m "Keep the keychain loader in an overlay"
layr overlay create zulip --readers @sapdev      # or --readers alice,bob
layr overlay move --purge zulip .zulip.env
layr save -m "Keep the Zulip key in an overlay"
```

Both states change only overlays, so a protected `main` takes them. A new line from `main` shows
the overlays its owner reads: a member of `sapdev` gets `.zulip.env`, other users get neither file.
A reader adds an overlay to an older line with `layr overlay use --add zulip` in that line.


## 10. Overlays in stores and peer sync

Every push includes the overlays the caller can discover.
Each store remote uses a separate age layer identity for each overlay.
Shared files still use the store identity.
Only readers with a slot can decrypt an overlay.
A machine without a slot can encrypt uploads to the signed layer recipient.

Local users and groups remain readers through the service.
They do not get store slots.
Add a machine reader with `machine:<UUID>`.
Its public key must be in the project's trust records before push can create its slot.
Add a person reader with `person:<login>=<age recipient>`.
The signed readers record binds the login to that recipient.
For example:

```sh
layr overlay readers secrets alice machine:01234567-1234-7123-8123-0123456789ab person:ann@example.com=age1...
```

Use the actual machine UUID and person recipient.
The owner remains a local reader.
Reader removal starts a new epoch at the next push.
The new epoch has a new identity and slots for the remaining readers.
The removed reader can still read data copied during an earlier epoch.

Each epoch holds its records, states and slots under `overlays/<id>/epochs/<epoch>/`.
The epoch is a UUID.
For a visible overlay, the public `overlay.key` object contains the signed recipient and its project and store bindings.
All remote objects use conditional create. No object is replaced.
Fetch and clone import an overlay only when a slot opens.
Without a slot for a visible overlay, they report `overlay <name>: no key on this machine` and import shared files.
Mount merges the overlay files into the line view when a slot opens.

The owner paper slot uses the store's paper opener.
The local store pin retains its public recipient, never the paper key.
For an older store, run push once with `LAYR_PAPER_KEY_FILE` to add the paper slot.
Without that public recipient, push reports the missing paper slot and continues.

Peer transfers use the existing signed records and snapshot streams.
The receiving service enforces the signed overlay readers.
The default peers are trusted machine readers of the overlay.
Set `overlay.<name>.peers` to an array of machine UUIDs to select other peers.
For example, use `layr config overlay.secrets.peers '["<machine UUID>"]'`.
A peer that is not permitted receives the shared set.
For a visible overlay, the peer also receives a report of the omitted overlay.
Root on a peer can read its disk. Service readers do not restrict root.

The creator machine is the first key manager.
Use `layr overlay managers secrets machine:<first UUID> machine:<second UUID>` to add a second manager.
The command replaces the manager set, so include the existing manager.
A manager that remains in the set must sign removal of another manager.
Only these machines can change readers or publish layer keys.
Other trusted machines can push to the published recipient.
The report names an unauthorized key signer.
Use two managers. If both are lost, create a new overlay and move the files.

## 11. Hidden overlays

A private overlay is visible to all project readers. Its files are visible only to its readers.
A hidden overlay is visible only to its readers. Other project readers see no name, count, reader, pattern, or operation for it.

```sh
layr overlay create secrets --hidden --readers alice
layr overlay move secrets .env
layr save -m 'Save credentials'
```

Hidden overlays use `follow` mode. Each line uses the overlay's own line history.
Parent states contain no hidden layer identifiers.
The overlay's own signed records hold its parent project identifier, readers, peer settings, and selection for each line.
Parent records and configuration contain no hidden metadata.
Read-only commands on another user's line use the shared snapshot and layers the caller can read.
They silently exclude unread hidden layers. Unread visible private overlays keep their access refusal.
Working folders retain the existing owner-only permissions.
A reader sees the `hidden` badge in `layr overlay list`.
Use `--json` for the same visibility rules in structured output.

A store encrypts hidden key records to the overlay recipient.
A new reader opens its machine or person slot and then discovers the overlay.
The next push creates the new reader's slot.
A non-reader receives no omission row or count for a hidden overlay.
Store access reveals encrypted objects under random overlay identifiers and their sizes.
It does not reveal overlay names or contents.

Use `layr config overlay.secrets.peers '["<machine UUID>"]'` to permit a peer.
The hidden overlay stores this setting in its own log.
A peer without permission receives no hidden name or record.
Root on a permitted peer can read its disk.

The secret detector checks hidden values only on readers' machines.
A machine without the reader key cannot check those values.

```sh
layr overlay show-to-project secrets
```

This command records `overlay.show-to-project` in the overlay log.
It then publishes the normal overlay metadata and authority to the parent project.
Project readers can now see that the overlay exists. Its files still require overlay read access.
A visible overlay cannot become hidden. Configuration cannot reverse this change.

## 12. Public projects

`layr remote public <remote> on` publishes only the parent project's reader slot.
Overlay access defaults to private. An explicit `layr overlay mode <name> public`
can publish that visible overlay with its own identity and public slot, only in a public
project. `service` is also supported within the project mode. See [access modes](access-modes.md).
Visible private overlays keep their names in shared metadata, but their files stay with
their readers. Hidden overlays keep their names, counts, state identifiers and history
out of the public reader's records and output, under the same rule 8.
A public-only clone or mount reads shared project files and explicitly public overlays.
Changing the parent project's visibility does not change an overlay's readers.
