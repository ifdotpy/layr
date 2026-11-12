# Cache overlays and remotes per layer

Status: design approved by the owner on 2026-10-27. Section 9 says which slice builds which part.
Built: slices a to d (sections 11 to 14).
This file extends [overlays](overlays.md) (the model of layers, section 9 = what is built) and
replaces the cache part of [design](design.md#5-warm-caches) as the slices land.

## 1. Problems

Caches today are nested layers that a state carries (`StateRec.caches`, `save --caches`). Real use
in the Studio project found these problems:

| Problem | Cause |
| --- | --- |
| A new line from a line starts cold after `save --caches` on that line | The caches go to an automatic state; a line name selects the last named state, which has none. |
| A line from a named state starts cold with no warning | The same: caches belong to states, and only some states have them. |
| A line from an automatic state holds ignored files in its index | Lines are started from automatic states only to get their caches. |
| Caches pass between lines only at `line new` | A state is fixed; there is no way to bring a newer cache into an existing line. |
| Caches diverge after a merge and fill the disk; no size view, no limit | Each line has its own copy; nothing removes old output. |
| Every cache snapshot stays for 5 states | `retention.caches` counts states, not lines or bytes. |

## 2. Model

Owner decision 2026-11-11. A cache is an ordinary overlay: an overlay of the model in
[overlays](overlays.md) section 1. No special cache concept remains: no cache flags, no `cache.`
name prefix, no separate code path. Slices 3 and 6 of [overlays](overlays.md) section 1a build most
of it; "Built today" below says how the code still differs.

- A cache is an overlay such as `target.macos-aarch64` with `folder=target`, `quiet`, `history=last`,
  `merge=union-theirs`, `platform=macos-aarch64` and `trust=protected`. Build output of each
  platform is its own overlay. Lines and overlays are independent: a line does not own a cache, and
  a cache does not own lines.
- A line has the same name on every overlay. One state of a line holds its part on each overlay,
  the cache overlay too. A view joins a line and a set of overlays.
- `history=last` on an overlay: older states of a line keep their parts on other overlays; their
  part on this overlay is dropped.
- `fetch` takes lines and overlays: for example, a merge runner takes the line `main` with the
  base and the `target.linux-x86_64` overlay.
- Storage: cache overlays go into v2 objects ([objects](objects.md)) like any overlay. A state
  record names one tree and one times table per overlay.
- Space: layered storage and the cleanup of old layers (`history=last`) save space. No size cap
  applies to a cache: the owner rejects `gc lru:<size>` for caches.

Built today:

- `save -m` saves the part of each build-output overlay that changed, and the state names it
  (`parts`). The first named save of a `regenerable` folder makes the overlay of this platform,
  `<folder>.<platform>`. `line new <state>` takes the part that the state names.
- The cache flags of transfers (`--caches`, `--no-caches`, `--caches-of` on push, fetch, pull,
  sync and clone) are removed: each fails with exit status 2 and names `--line` or `--overlay`.
  `save --caches`, `merge --caches-only` and `fsck --caches` are removed too: they are
  `save --overlays`, `merge --overlays-only` and `fsck --overlays` ([commands](commands.md), `save`).
- A line is the same line on every overlay: the overlay store holds `main` with the id and name of
  `main` ([overlays](overlays.md) section 1a, "Lines on overlays as built"). A `cache.<folder>` overlay
  of an older layr stays the overlay of its folder on every platform, with its old line names
  (`line-<line id>.<os>-<arch>`, section 6) until each line writes it again or `overlay
  migrate-parts` runs. To move a store to one overlay per platform, see [overlays](overlays.md)
  section 1a, "Slice 6 as built".
- No setting marks a build-output overlay: the `cache` flag is gone (2026-11-12). Its rules read
  the properties `platform`, `trust`, `history` and `folder` ([overlays](overlays.md) section 1a).
  `src/cache_overlay.rs` keeps the build-output code: which part a line takes, and the key parts of
  a build.

The properties as built:

- An overlay has **properties**. Each property has a default that keeps the behaviour of section 9
  of [overlays](overlays.md):

  | Property | Values | Default | Meaning |
  | --- | --- | --- | --- |
  | `history` | `all`, `last`, `<N>` | `all` | How many states each overlay line keeps. `last` is `1`. Older states of the line are deleted at the next state of the line and by `gc`. |
  | `quiet` | on or off | off | The overlay files are not in `status`, `diff` and `add`. Their changes go into the overlay without the index (section 4). |
  | `merge` | `three-way`, `union-theirs` | `three-way` | How a merge of a line combines the follow overlay. `union-theirs`: the result has every file of both sides; on the same path the incoming line wins; no conflicts. |
  | `gc` | `none`, `lru:<size>` | `none` | Built, but not for caches (owner decision 2026-11-11: no size cap for caches). `gc` removes build units from the working folders of the overlay, least recently read first, until the overlay folder of each line is at most `<size>` (`500M`, `20G`). A unit is all files of one cargo hash in one profile (`deps`, `.fingerprint`, `build`, `incremental`); any other file is its own unit. Folders that this empties go too. |
  | `folder` | a path | none | A **folder overlay**: the overlay is the whole folder at that path (section 3). |
  | `platform` | a platform key (`<os>-<arch>`) or none | none | The overlay of one platform (overlays.md section 1a, slice 3): shown, fetched and pushed only on that platform; a plain fetch takes only the caches of `main`, the protected lines and the save-back lines. Each part also records the key of its build (section 6); a line takes a part only when that key fits. |
  | `trust` | `protected`, `readers` | `protected` for a folder overlay | `protected`: a part that another user saved is taken only from a protected line (design.md section 5). `readers`: any part that the user reads. |

- A **cache overlay** is an overlay with `folder=<path>`, `quiet`, `history=last`,
  `merge=union-theirs`, `platform=<key>` and `trust=protected`, in follow mode. Its readers are its owner and the machines
  that the project trusts (owner decision 2026-10-27), so store copies and peer sync give caches
  only to those machines.
- **`regenerable`** in `.layrattributes` is a short form: the folder that it marks gets the overlay
  of this platform with that folder and the defaults above, the first time a named save (or
  `layr save --overlays`) saves it.
  The project setting `regenerable` (`layr config regenerable '["<path>"]'`) names cache paths for
  every line; a new line gets an empty cache of such a path at once.
- Each line is a line of the overlay too, with its own name. So **a line has caches**, not only a
  state: `line new X <line>` takes the newest cache of that line, the head of the line on the overlay.

## 3. Folder overlays

A folder overlay owns one folder of the tree, all files below it. The folder is the unit, so a
state of the overlay and the copy into a line are one operation each, at any number of files.

| Operation | btrfs | APFS |
| --- | --- | --- |
| the folder in a working folder | a nested subvolume | a nested layer: a folder with the layr tree marker |
| a state of the overlay | a read-only snapshot of the nested subvolume | one clone of the nested layer (`clonefile`); file by file only for hard links or special files |
| a line shows a state | `rmdir` of the placeholder, a writable snapshot of the state at the path | the same with a clone |
| a snapshot of the line | the folder is an empty placeholder, as for caches today | the folder is an empty folder, as for caches today |

So `status`, `diff` and automatic states never see the files of a folder overlay: the scans and
snapshots of the line stop at the nested layer, as they do for caches today. A folder overlay is
quiet by construction. The edges of section 6 of [overlays](overlays.md) apply (`rename()` across
the edge gives EXDEV). For a build output folder that is not a problem: tools write inside it.

A folder overlay is not one of the overlays of the view of a line folder. Its files never enter
the base, the split of a named save or an assembled tree. The state that a line shows is the head
of the line on the overlay.

A folder overlay must be ignored by the ignore rules and must not hold files of the line's head
state, as for caches today (`design.md` section 5). A folder that holds versioned files stays a
plain folder; `save --caches` says so.

A cache overlay applies only in a line where its folder is a cache path: the folder is named by the
machine setting `regenerable`, or a `.layrattributes` file marks it `regenerable` and the ignore
rules ignore it. The folder may be absent. Other lines ignore the overlay:

- `overlay list` shows `not a cache path in this line` for it, with no repair hint.
- `overlay repair` of it exits 0 and prints the same message. It changes nothing.
- A line's `line new`, `merge`, `follow` and `gc lru:<size>` skip the folder.
- A plain `save` and `save --caches` of another line do not touch it.

The `overlay list` and `overlay repair` rule covers every folder overlay. The skip rule covers
cache overlays only: a folder overlay made with `overlay create --folder` is not skipped.

## 4. Commands

| Command | Effect |
| --- | --- |
| `layr overlay create <name> [--history all\|last\|<N>] [--quiet] [--merge three-way\|union-theirs] [--gc none\|lru:<size>] [--folder <path>] [--platform]` | Makes an overlay with these properties. |
| `layr overlay set <name> [--history ...] [--quiet\|--no-quiet] [--merge ...] [--gc ...] [--platform\|--no-platform]` | Changes properties. The owner only. A record `overlay.set` keeps the change. `--folder` cannot change. |
| `layr overlay list`, `layr overlay show <name> --json` | Also show the properties. |
| `layr save --overlays` | For each cache path of the line (`regenerable`, in `.layrattributes` or the project setting): makes its cache overlay when missing, makes the folder a nested layer when a tool made a plain folder, and saves a new state of the overlay line of this line (one clone). Older states go by `history`. |
| `layr save -m <message>` | Also saves the quiet file overlays of the stack from the working folder (section 4.1). |
| `layr line new <name> <line>` | The new line takes the newest state of each cache overlay of the source line. |
| `layr line new <name> <state>` | The new line takes, for each cache overlay, the newest state saved at that state or at its nearest ancestor (the rule of `warm_cache` today). It prints `no cache for <path>` when there is none. |
| `layr merge <line>` | Merges each follow overlay of the source line, with the overlay's `merge` property. A cache overlay unions the files into the target's folder. |
| `layr overlay repair <overlay>` | In the current line, the folder of that folder overlay: a plain folder becomes a cache again; a cache with versioned files becomes a plain folder. In a line where the folder is no cache path, it prints `not a cache path in this line` and exits 0 (section 3). |
| `layr overlay remove <cache overlay>` | Removes the cache overlay from the project (section 18). Refuses a versioned overlay. |
| `layr overlay list` | Also shows, for each folder overlay in a line, the state of its folder and its saved state; add `--size` to recursively measure its apparent file size (was `layr cache list -v`). |
| `layr cache add\|remove\|list\|repair\|pull` | Removed. Section 15 names the replacements. |
| `layr gc [-n] [--dedup]` | Also applies `history` to overlay lines and `gc lru:<size>` to working folders. With `--dedup`, also shares equal cache content (section 16). |
| `layr overlay remote add <overlay> <remote> <git URL>` | The overlay's history goes to its own git repository (section 7). Refused for a cache overlay. |
| `layr push [--dry-run]` | Pushes every layer to each of its remotes (section 7). `--dry-run` lists layer, remote and size. |

`layr overlay create -q` keeps its meaning (no output). The long form `--quiet` of `overlay create`
was the same flag; it now sets the property. No test or caller used the long form.

### 4.1 Quiet file overlays

A quiet file overlay (no `folder`) holds named files, as other file overlays do. Its files do not
appear in `status`, `diff` or `add`, and `add -A` never stages them. A named save takes the
current content of each of its files from the working folder (not from the index): a change, a
delete, and the paths of `overlay move`. Routing patterns are not built, so a new file goes to a
quiet file overlay only through `overlay move`.

## 5. Merges and history

- A merge with `union-theirs` makes a state of the target's overlay line: a writable snapshot of
  the target's state, reflink copies of every file of the incoming state, read-only. Parents: the
  target's state and the incoming state. No conflicts. A fast-forward stays a fast-forward.
- For a folder overlay, the merge also updates the target's working folder: a clone of each
  incoming file over the target's folder. A build that runs in the target folder at that moment
  can see a mix of files; the build tool then rebuilds what is stale.
- `history=<N>` keeps the newest N states on the first-parent chain of each overlay line, the
  heads of all overlay lines and the empty first state. It deletes the other states of the
  overlay: their snapshots go, their records stay. A three-way merge needs its base state; when
  `history` deleted the base, the merge refuses and names `--merge union-theirs`.
- Records of the undo window do not keep states of an overlay with a `history` limit. `undo` of a
  save that made such a state still moves the overlay line back while the state exists.

## 6. Platform key

A cache is build output for one platform. Today a state records `<os>-<arch>`. A cache overlay
with `platform` records a key in each state:

```
<os>-<arch>[-<hash>]
```

`<hash>` is the first 16 hex digits of the SHA-256 of the key parts, each a name and a short hash
of its value (a missing file or an unset variable is `-`):

- `file:<path>`: each file that the setting `overlay.<name>.key-files` lists, from the line's
  working folder (for example `["Cargo.lock", "rust-toolchain.toml"]`).
- `env:<NAME>`: each environment variable that changes a build in the folder. The folder's cache
  adapter names them (cargo: `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTDOCFLAGS`,
  `CARGO_INCREMENTAL`, `CARGO_PROFILE_*_DEBUG`, `CARGO_BUILD_TARGET`, `RUSTC_WRAPPER`,
  `RUSTC_WORKSPACE_WRAPPER`, `MACOSX_DEPLOYMENT_TARGET`, `IPHONEOS_DEPLOYMENT_TARGET`, `CC`, `CXX`,
  `CFLAGS`, `CXXFLAGS`, `LDFLAGS`, `AR`), and the project setting `cache.key-env` (a JSON array)
  adds names. A value that the project pins for its builds wins over the caller's (cargo: the `env`
  table of `[workspace.metadata.layr-retest]`, which retest and the gate apply).
- `toolchain`: the output of the adapter's toolchain command (cargo: `rustc -vV`), run as the line
  owner in an empty folder with copies of the files that select the toolchain
  (`rust-toolchain.toml`, `rust-toolchain`). One run per command.

Each cache state records its parts by name (`cache_key_parts`). A line that does not take the cache
of its source line names the parts that differ, for example `target: the cache of line main is not
taken (state 1a2b3c4): the key differs in env:MACOSX_DEPLOYMENT_TARGET`. A cache saved by an older
layr has no parts; its key differs once.

- `line new` and merges take a state only when its key equals the key of the target
  line (computed in that line at that time).
- Before 0.44, a line had one overlay line per platform in a `cache.<folder>` overlay:
  `line-<line id>.<os>-<arch>`. A `save --caches` on
  one platform makes a state of that platform's overlay line, so it does not replace the cache of
  another platform, and `history` applies per platform. `line new`, merges and follow passes read
  the overlay line of their own platform. An overlay line of layr 0.24 and older (`line-<line id>`)
  takes the name of its head's platform at its first write; until then a read takes it when its
  head has this platform.
- Remotes (section 7): a layr remote carries the states of every platform; a receiving machine
  imports only the states of its own `<os>-<arch>`.

`layr fsck` accepts a missing overlay head tree from another platform when its record marks
the tree absent and not deleted. It reports this as data that is not local. A head marked
present still needs its tree. A missing head for the local platform remains an error.

## 7. Remotes per layer

There are two kinds of remote:

- A **layr remote** (a store: `file://`, `s3://`, `layr+https://`; or a peer: `ssh://`) takes the
  whole repository: the shared set and every overlay, each overlay encrypted for its readers, as
  [overlay sync](overlay-sync-design.md) decides. Cache overlays travel too. A plain `layr fetch <remote>`
  takes the records of a cache overlay and only the caches of `main`, the protected lines and the
  save-back lines; `fetch --overlay <cache>`, `fetch --line <line>` and a clone take the others
  ([remotes](remotes.md)). A receiving machine
  imports a cache state only for its own platform, and only a state that a line here can take
  (`usable_cache`: shared, or of the caller). `layr clone <url> --line <line>[,<line>]` and
  `layr fetch <remote> --line ...` (the old `--caches-of` is removed) restore only the caches that `line new X <line>` takes for
  those lines: the newest cache of the line, and the newest cache of this platform saved with each machine's
  head of it or that head's nearest ancestor (a CI clone for one commit; a runner whose own main is older than the main
  of a server queue). `layr fetch <remote> --overlay base` (the old `--no-caches` is removed) reads no cache overlay at all: a merge
  machine builds nothing, and every new cache of the check runners was gigabytes per fetch. `--heads` restores only the heads of the lines (and what an
  undo of a line delete needs): the other states of the history stay on the remote, the clone names
  them in `states-left` of the remote's local folder, and a command that reads one restores it
  from there. A plain clone restores the whole history and works offline. A cache state that a push removes while a fetch runs, and a
  cache overlay that a fetch cannot read, omit; the fetch goes on. A store remote keeps only the cache states that
  the records keep (`src/store_remote/sweep.rs`): the push of a cache overlay deletes the trees of
  the states that its records know and do not keep (beyond `history` on each line and platform, of
  deleted lines), and the data parts that no kept tree links, parts first. A tree stays when its
  state is not known here yet (another machine pushed it), when a kept tree is a delta of it, or
  when it is the nearest pushed ancestor of a kept state whose tree is not there yet (a push in
  flight on another machine reuses its parts). The daemon runs the same pass for each store remote
  of each project every hour, so a remote that nobody pushes to loses its dead caches too (a remote
  that cannot be reached is tried at the next pass). `layr push <name> --no-cache-sweep` defers
  the ordinary cache cleanup to a later push or daemon pass. Key-epoch cleanup still runs. A
  remote can refuse a delete (a Layr Cloud gateway GC token deletes only state data parts): the
  first refusal stops the pass with a warning, the push goes on, and the rest stays for a later pass.
  `layr fsck --caches --remote <name>` lists what
  the pass would delete (exit 1 when there is something); `layr gc --remote <name>` runs it. A
  key change (a reader removal, a closer access mode) starts a new key epoch, and a reader takes
  only the newest published key: once the key of the new epoch is published, the push and the pass
  delete all cache states of the older epochs whose keys were in the same key listing. An epoch
  whose key is not published yet (a key change in flight) stays. Records and slots of an old
  epoch stay; they are small.
- A **git remote** takes one layer. The shared set goes to the project's git remotes (`origin`),
  as today. An overlay goes to its own git repository: `layr overlay remote add <overlay> <url>`
  records `overlay.<name>.git.<remote>` in the overlay's own records, so only its readers see the
  URL. The branch of an overlay line is the name of the line that uses it. A cache overlay never
  goes to git: `overlay remote add` refuses it.
- An overlay remote is a remote plus an optional branch name, so that the follow-up below fits
  without a change of the records.

Planned follow-up (owner, 2026-10-27; not in slices a to d): `layr overlay remote add <overlay>
<remote> [--branch <name>] [--encrypt]`. The remote can be a separate repository (overlay lines map
to its branches) or an orphan branch of the same repository (for example `overlay/notes` in
`origin`) that holds only the overlay files. Git has no read rights per branch, so an overlay whose
readers differ from the readers of the repository needs `--encrypt` (age, for its readers) on a
branch of the same repository; a plain `git clone` also gets that branch unless its refspec leaves
it out. Caches never go to git in either form.

`layr push` without a remote name pushes the current line: the repository to each layr remote,
and each layer to each of its git remotes. `layr push --dry-run` prints one row per layer and
remote:

```
layer      remote   kind  size
shared     origin   git   12 commits, 1.4 MiB
keys       keys-gh  git   2 commits, 3 KiB
target     archive  layr  1 state, 812 MiB (macos-aarch64-...)
```

## 8. Compatibility

- **Stores with old caches.** The old cache snapshots of states (`StateRec.caches`) warm no line,
  travel to no peer and stay out of store pushes. `save --caches` and `gc` move them to the trash;
  the states stay. `retention.caches` is gone.
- **`save --caches`** keeps its name and meaning for the user: it saves the caches of the line.
  It writes cache overlay states and no more `StateRec.caches`. On a protected line with no
  changes it no longer needs to give the caches to the head state: the caches belong to the line.
- **`regenerable`** keeps its meaning. The `cache` commands are removed (section 15). `overlay repair`
  turns a plain folder at a cache path into a nested layer, as `cache repair` did.
- **`sync --caches`** (live move) carries cache overlays as it carries other overlays. The old
  cache snapshots of states do not travel.
- **Older layr binaries** read the overlay records as ordinary overlay records: properties are
  extra fields of `overlay.<name>`. An older binary shows a folder overlay as an overlay with no
  files and does not take its states into new lines.

## 9. Slices

Each slice is one pull request that leaves `main` green.

| Slice | Content | Tests |
| --- | --- | --- |
| a | Properties `history`, `quiet`, `merge`, `gc` (stored and shown); `overlay create` and `overlay set` options; `merge=union-theirs` for follow overlays; `history` applied after each new overlay state and by `gc`; quiet file overlays out of `status`, `diff` and `add`, saved from the working folder. | unit tests; `tests/t58-overlay-properties.sh` (macOS and Linux, one user) |
| b | Folder overlays; `save --caches` writes cache overlay states; `line new` from a line or state takes them; `cache list` with sizes; `cache pull`; `gc lru:<size>`. | `tests/t59-cache-overlays.sh`; `t09-caches`, `t14-warm-caches`, `t15-regenerable` stay green |
| c | `overlay remote add\|list\|remove`; `layr push` to every remote of every layer; `--dry-run`. | `tests/t60-layer-remotes.sh` with local bare git repositories |
| d | Platform key (`key-files`); layr remotes import only the own platform's cache states. | unit tests for the key; store push and fetch between two local stores |

## 10. Open questions

1. **Unreviewed build output in protected lines.** Decided by the owner on 2026-10-27: a merge
   into a protected line carries the source line's caches only when that merge met the line's
   protection: each required check passed on the new head, and the merged state has its
   approvals, the same rule as for the code, with no bypass (root, an up-to-date merge).
   Unprotected lines take the caches of every merge.
2. **Readers of cache overlays.** Decided by the owner on 2026-10-27: the owner and the machines
   the project trusts, not every user. Each `save --caches` adds machines that the project began
   to trust since (on a key manager of the overlay).
3. **Toolchain in the platform key.** layr hashes the files that `key-files` names. A toolchain
   that no file names (a system compiler) is not in the key.
4. **LRU on APFS.** macOS updates access times lazily; `lru` uses the access time and the
   modification time, whichever is newer. Owner decision 2026-11-11: no size cap for caches;
   `history=last` and layered storage save space.

## 11. Built: slice a

Built on 2026-10-27: the properties `history`, `quiet`, `merge` and `gc` for file overlays. Tests:
`tests/t58-overlay-properties.sh` (one user; macOS and Linux) and the unit tests in
`src/overlay.rs`.

- `overlay create` takes `--history`, `--quiet`, `--merge` and `--gc`; `overlay set <name>`
  changes them (the owner only). Each property is the setting `overlay.<name>.<property>`
  (`history`: a number; `quiet`: true; `merge`: `union-theirs`; `gc`: `lru:<bytes>`). A default
  is no setting. A hidden overlay keeps them in its own records.
- `overlay list` shows the properties that differ from the default; `overlay list --json` and
  `overlay show <name> --json` have a `properties` object.
- `history`: applied after each named save and merge that moves an overlay line, after
  `overlay set --history`, and by `gc`. `overlay log` still lists the deleted states (their
  records stay); `overlay show` of such a state fails.
- `merge=union-theirs`: as section 5. A three-way merge whose base `history` deleted refuses and
  names `overlay set <name> --merge union-theirs`.
- `quiet`: as section 4.1. A pending `overlay move` into a quiet overlay is not in `status`
  either. The summary of `save` still counts the quiet files.
- `gc=lru:<size>` is stored and shown; slice b applies it (it needs folder overlays).

## 12. Built: slice b

Built on 2026-10-27: folder overlays and caches as cache overlays. Tests:
`tests/t59-cache-overlays.sh` (one user; macOS and Linux) and unit tests in
`src/cache_overlay.rs`. The model is in `src/cache_overlay.rs`.

- `overlay create <name> --folder <path>` makes a folder overlay (follow mode, not hidden; one
  overlay per folder). `overlay list` shows `folder=<path>`. `overlay use` and `overlay move`
  refuse a folder overlay.
- `save --caches` makes the cache overlay `cache.<folder>` (`/` as `.`) of each cache path when it
  is missing: folder overlay, readers: the trusted machines, `history=last`, `quiet`, `merge=union-theirs`, and (before
  0.44) the setting `overlay.<name>.cache = true`. It then saves one snapshot of the folder as a state of the
  line's overlay line. Each state records the user, the platform (`<os>-<arch>`) and whether the
  line is protected, with the rules of `design.md` section 5 for who may take it.
- `line new X <line>` takes the newest cache state of that line (dogfood item 10). When that cache
  was built at an older head of the line and a shared cache was saved with the head itself (by a
  protected line, or by a check of that head: `<line>.checks-<platform>`, docs/checks.md), it takes
  the cache of the head. A user's cache of an unprotected line at the head does not count. A merge of a line takes
  its caches by the same rule.
  `line new X <state>` takes the cache of the state, else of its nearest ancestor that has one
  (item 21). For each state on that walk it takes first the newest cache saved with it
  (`cache_of`; a cache saved with an automatic state counts for its parent, the line head), and
  else the newest cache of the line that made the state. A state imported from Git has no line,
  so without `cache_of` the walk ends at an old state of another line and takes its old cache. A cache overlay goes into a new line only while
  its folder is a cache path there (`regenerable`) and the ignore rules ignore it.
  Without a cache overlay state, the old caches of the state apply as before. With one, the
  cache overlay state replaces an old cache of the state (`states/<id>@<folder>`) or a copy the
  state holds: the new line gets the newest cache overlay state.
- `layr merge <line>` then brings the merged line's caches into the target (section 5): a
  fast-forward of the target's overlay line, or a `union-theirs` state; the incoming files are
  copied over the target's folder; an empty folder gets one snapshot. A failure here is a
  warning; the merge stays. Into a protected line only when the merge met its protection (each
  required check passed on the new head, the required approvals of the merged state); otherwise
  the merge prints `Caches of '<line>' not merged`.
- Cache overlay readers: the owner and every machine the project trusts (`machine:<UUID>`), not
  `*`. Local users take caches by the rules of `design.md` section 5 (their own, or saved on a
  protected line). A key manager adds a machine trusted since the overlay was made at its next
  `save --caches` or push of the overlay, so a push right after `remote trust` gives the new
  machine the caches that exist.
- `layr cache pull <line or state>` (removed in section 15): the same for one line, without a merge (item 24).
- `layr cache list -v` (removed in section 15) showed the size of each cache folder and the overlay state
  the line saved; `overlay list` shows both.
- `layr fsck --caches` reports the cache data that nothing keeps, by overlay, with its bytes: states
  beyond `history` or of deleted lines, overlay lines of deleted lines, and old cache snapshots of
  states. The operations that make cache data dead delete it, so the report is 0 after
  them; it exits 1 when it finds some. It only reads.
- `save --caches` applies `gc=lru:<size>` to the folders it saves (before, only `gc` did), and `gc` applies it to the folder of each line (the newer of access and change time;
  apparent sizes) and deletes the cache states of deleted lines.
- `line delete` also deletes the lines of the line on the overlays (each overlay; before 0.44 only
  the cache overlay lines `line-<id>.<platform>`), and their states at once (the overlay's `history` keeps only the heads of the overlay lines
  that stay and the states that lines show). `undo` of that delete restores the line but not its
  cache overlay lines: the next `save --caches` makes new ones. `gc` and `fsck --repair` remove cache
  overlay lines of deleted lines that older versions left behind.
  `fsck` marks such a problem "(of a deleted line)".
- The secret detector does not read folder overlays: they hold build output.

Limits of slice b:

- `save --caches` still also writes the old caches of a state (`StateRec.caches`), so `try`, the
  live move (`sync --caches`) and older binaries keep working. That is two snapshots per folder.
  Slice d moves the live move to cache overlays; then the old write can go (done: `save --caches`
  writes cache overlays only).
- Only `layr merge <line>` merges caches; the merge queue and `merge <state>` do not.
- A union never deletes a file of the target's folder.
- Tested with one user on macOS. The Linux root suite (t14, t15 with several users) is not run
  here; their checks of `cache list` output and of old cache records are kept by design.

## 13. Built: slice c

Built on 2026-10-27: git remotes per overlay and `layr push` of every layer. Tests:
`tests/t60-layer-remotes.sh` (one user, local bare repositories). The code is in
`src/cmd/layerpush.rs`.

- `layr overlay remote add <overlay> <remote> <git URL>`, `overlay remote list <overlay>` and
  `overlay remote remove <overlay> <remote>`. The owner adds and removes; readers list. The
  setting `overlay.<name>.git.<remote>` = `{"url": ..., "branch": null}` is in the overlay's own
  records. `branch` is kept for the planned follow-up of section 7 and not used yet. A folder
  overlay is refused.
- `layr push` without a remote name pushes the current line: the repository to each layr remote
  (`layr push <remote>` of each store and peer), the line to each git remote of the project
  (`layr git push <remote> <line>`), and the overlay line of each overlay to each of its git
  remotes, to the branch of the line's name. The overlay gets its own git store
  (`<project>%<overlay>`), its own export records, and commits of its own history.
- `layr push --dry-run` prints `layer remote kind size`: for git the states that the remote does
  not have yet (from the export records) and the size of the layer's tree; for a layr remote one
  row for the repository and one per cache overlay with its size and platform.
- A failed remote is reported and the others still run; the exit code is 1.
- The secret detector does not check an overlay's git push: an overlay is where private files
  belong. Its remote must be as private as its readers.

## 14. Built: slice d

Built on 2026-10-27: the platform key and the platform filter of layr remotes. Tests:
`tests/t61-cache-platform.sh` (one user; a local store and a clone with the paper key) and unit
tests in `src/cache_overlay.rs`.

- `overlay create|set <name> --key-files <a,b>|none` sets `overlay.<name>.key-files`. The project
  setting `cache.key-files` (a JSON array) gives each new cache overlay its key files, for example
  `layr config cache.key-files '["Cargo.lock", "rust-toolchain.toml"]'`. `overlay list` shows them.
- Each cache state records its key; `line new` and `merge` take a state only when its
  key equals the key computed in the target folder at that time. A changed lock file makes the next
  line cold until the next `save --caches`.
- A store fetch or clone does not import a cache overlay state of another `<os>-<arch>`; the report
  names it (`cache state <id>: platform <key>`). The states of the same `<os>-<arch>` with other key
  files are imported; lines just do not take them.

Limits of slice d:

- Peer sync (`ssh://`) has no platform filter. A peer gets an overlay only when the overlay
  permits it (section 10 of [overlays](overlays.md)); the readers of a cache overlay are the
  machines the project trusts, so trusted peers get caches and other peers do not.
- A store encrypts an overlay for its readers: a trusted machine opens a cache overlay with its
  machine slot, which the next push makes; the owner's paper key opens it too.
- The live move (`sync --caches`) carries the cache overlays; the old state caches are gone.

## 15. Command change (owner decision, 2026-10-27)

A cache is an overlay. The `layr cache` command group is removed, with no hint. The replacements:

| Removed form | Replacement |
| --- | --- |
| `layr cache list [-v]` | `layr overlay list` |
| `layr cache add <path>` | `regenerable` in `.layrattributes`, then `layr save --overlays` |
| `layr cache remove <path>` | `-regenerable` in `.layrattributes`, or `layr config regenerable` without the path |
| `layr cache repair` | `layr overlay repair <overlay>` |
| `layr cache pull <line or state>` | `layr line new <name> <line>`, or `layr merge <line>` |

The lines that carry caches are unchanged:
`line new` takes them, and `merge` brings them back.

- `overlay list` does not walk cache contents by default. Add `--size` to measure the apparent size of active cache folders (without following links or entering other layers). Text shows `-` when size is not computed; JSON omits `size` unless measured, and a measured empty cache is `0`. `--json --size` combines both options.
- `overlay list` shows folder overlays only. A regenerable folder with no overlay has no row. It
  has no overlay until its first `save --caches`, and a folder with versioned files never gets one.
  `save --caches` prints why a folder is no cache.
- `overlay repair <overlay>` needs the overlay. It repairs one folder of the current line.
- `cache add` converted the folder at once. With `regenerable`, `save --caches` converts it.
- `overlay move` puts files in a file overlay. It does not make a folder a cache.
- There is no `overlay delete`. The owner has not decided how to remove a cache overlay and its
  history. `-regenerable` stops new saves of the folder; the overlay keeps its states.
- `pull_caches` stays in the library: `merge` calls it.

## 16. Deduplication in `gc`

A clone of a cache folder stops sharing as soon as a build tool writes a whole new file, even when
the new file equals the old one. `layr gc --dedup` shares such content again, on APFS and on btrfs.

- `layr gc --dedup` runs it and prints `shared <bytes> of disk in <n> cache file(s), <seconds> s` (physical bytes freed, and the wall clock of the pass).
  `layr gc -n --dedup` changes nothing and prints `would share ...`. Plain `layr gc` does not run it and prints no share line.
- Scope: one cache overlay at a time (`target` and `.layr-retest` never share with each other, and projects never do). In an overlay: the files in the folders of folder overlays in the working folders of the lines of the
  project. Their content is matched with the files of cache states (`states/<id>@<path>`) and of
  cache overlay states of the project. Those states are read-only, so they are sources only: gc
  changes only files in the working folders of lines. Nothing outside these folders is read or
  changed. Symbolic links are not followed. A file with more than one hard link, and a file under
  4 KiB, is skipped.
- Locks: the pass runs after `gc` has released the project lock, as `save --caches` copies caches
  before its lock. A save or a build in a line can run at the same time.
- APFS: whole files only. Files of equal size are hashed (SHA-256) and compared byte by byte. The
  duplicate is replaced by a `clonefile` of one copy, made under a temporary name in the same
  folder and moved over the duplicate with an atomic rename. The new file takes mode, owner,
  extended attributes and mtime of the duplicate (cargo reads mtimes). Before the rename gc checks
  that inode, size and mtime of the duplicate and of the source are as at the scan, and skips the
  file when they changed. A file that already shares its extents with the source is skipped.
  APFS has no block-level API, so partial matches stay.
- btrfs: blocks, like `duperemove`, with `FIDEDUPERANGE` (in `sys.rs`). Each file is cut into
  128 KiB blocks, which are hashed. Equal blocks of two files, at any offset, become one range
  when they follow each other, and the kernel shares the range only if the bytes are equal, so a
  write during the call cannot lose data. 128 KiB is the largest uncompressed extent: 4 KiB blocks
  find more matches but split extents into many small ones. Ranges whose first byte already has the
  same device offset (`FIEMAP`) are skipped, so a second run shares nothing new.
- Cost: gc reads the cache files once to hash them. On APFS, files that already share all their
  extents count as one file, so only distinct content is read; equal hashes are compared byte by
  byte. A build right after gc is not slower: a clone costs nothing until cargo writes, and cargo
  writes new files. `OverlayFS` stores have no extents to share: the pass does nothing there.
  Measured on a cargo target of 190 MB per line in 5 lines (release build): 4.5 s.
- The block pass is plain code with tests on every platform. The ioctl runs on btrfs only: its
  test is `tests/t72-gc-dedup.sh` in the Linux job. On a file system without `FIDEDUPERANGE` the
  pass ends without a change.

## 17. `merge <line> --caches-only`

Built today. Owner decision 2026-11-11: the flag goes away; a merge takes the cache overlay like any
other overlay (section 2).

`layr merge <line> --caches-only` brings the caches of a line into the current line. It runs only
the cache step of `merge`: no file changes, no merge commit, and the head does not move. The
taken states are written as overlay line records, so their owners follow the overlay lines.

- `StateRec.cache_of` (new) names the project state whose working folder a cache snapshot was
  taken with. `save --caches` sets it. It creates no owner. A cache without it (an older save, or
  a union state) cannot pass a protected target.
- A protected target takes the caches only when its rule allows (checks and approvals of the
  head, as for a merge) and the squash-safe check passes: for each cache of the source, the
  changes from the merge base of the saved state and the target head to the saved state must
  hold the same content in the target head. Ignored paths and `skip_path` paths are not compared.
  A refusal names the first path that differs.
- An unprotected target takes the caches without the check.
- Test: `tests/t101-merge-caches-only.sh` (macOS and Linux CI, OverlayFS on ext4 and XFS).

## 18. Removing a cache overlay

`layr overlay remove <name>` removes a cache overlay from the project. The owner runs it. A
versioned overlay is refused: its files are history.

- The record is a `config` record. It clears the overlay's settings and keeps its empty first
  state id in `overlay.<name>.removed`. Other machines and `layr undo` see it.
- The overlay leaves the stack of every line, and pending moves into it are cancelled.
- The cache folder of each line on this machine becomes a plain folder with its files. A line
  that the caller may not save keeps its nested cache, and the command warns.
- A new `save --caches` of the folder makes a new overlay of that name.
- The states of the removed overlay are freed by the next `layr gc --repair`, as the states of a deleted
  line are. No line keeps them, so gc deletes all of them except the empty first state. Until gc runs, `layr undo` can restore the overlay.
- A plain folder that stays `regenerable` in a line becomes a cache again at the line's next
  snapshot, as any regenerable folder does. To keep it plain, remove `regenerable` from
  `.layrattributes` or from the machine setting first.
- Test: `tests/t110-overlay-remove.sh`.

## 19. Pruning cache folders

A cache folder of a line (`target`, `node_modules`) keeps the build output of old dependencies. A
cache adapter removes that output and keeps the live set: the entries that the project's standard
commands still need. Core stores the overlay property `prune` and runs the adapter; adapters are
separate crates (issue #167).

Crates:

- `crates/layr-adapter`: the interface (`CacheAdapter`, `Host`, `Outcome`). Core depends on it.
- `crates/layr-cache`: the cargo and pnpm adapters, and `adapters()`. Core does not depend on it.
- The binary (`src/main.rs`) registers `layr_cache::adapters()` at startup with `cache_hook::register`.

### 18.1 Adapters

The follow pass also runs each adapter's build (`warm`) before the prune (docs/follow.md): cargo builds
the `sweep` list with the project env, pnpm runs `pnpm install --frozen-lockfile` (online).

| Name | Folder | Project file next to it | Live set | Work |
| --- | --- | --- | --- | --- |
| `cargo` | `target` | `Cargo.toml` | the units of the `sweep` list of `[workspace.metadata.layr-retest]` | lists the units (no compile), then removes the other entries (section "Sweeping stale build output" of `docs/retest.md`) |
| `pnpm` | `node_modules` | `pnpm-lock.yaml` | the packages of the lockfile | `pnpm install --frozen-lockfile --offline --ignore-scripts`, then `pnpm prune --ignore-scripts` |

Rules every adapter keeps:

- Offline. No network: a package that the store lacks fails the step.
- Compiles nothing and runs no lifecycle script.
- A stale or failing state skips the folder. The result names the reason.
- The `pnpm` adapter copies `node_modules` with copy-on-write (`cp -cRp` on macOS, `cp -a
  --reflink=auto` elsewhere) before its first step, and puts the copy back when a step fails.
- The `cargo` adapter removes nothing before its listing succeeds. Cargo's freshness check can
  remove the outputs of the one unit that is not fresh before the listing stops; the next build
  rebuilds them.
- A busy cargo build lock skips its profile folder. The other profile folders are already swept.

### 18.2 Choosing an adapter

Without a setting, the adapter whose `detects` accepts the folder runs: `target` with `Cargo.toml`
next to it (cargo), or `node_modules` with `pnpm-lock.yaml` next to it (pnpm). The parent folder of
the cache folder is the project folder.

### 18.3 Command by hand

`layr retest prune <folder> [--adapter cargo|pnpm|none]` runs an adapter on a folder of the current
line and prints `<folder>: pruned with <adapter>, freed <N> MB` or `<folder>: skipped: <reason>`.

### 18.4 Overlay property `prune`

- Values: an adapter name, `none` (no hook), or `auto` (unset, the default). `overlay list` shows
  `prune=<name>`.
- Setting: `overlay.<name>.prune`. Owner only: `layr overlay set <name> --prune <name|none|auto>`.
- Core checks the form (lowercase letters, digits and `-`). It has no list of adapter names: an
  unknown name is logged when the hook runs, and the folder is not pruned.

### 18.5 The hook in core

```rust
impl Repo {
    pub fn prune_cache(&self, line: &Line, overlay: &str, folder: &str)  // never fails; logs one line
}
```

Core also has `prune_cache_folder(line, folder)` (the overlay is looked up from the folder). The
`save --caches` path calls it for each cache folder.

Process model:

- The adapter runs in a worker process: the layr binary started with `__cache-hook`, as the line
  owner (`privs::command_as`). The worker is its own process group. The adapter's processes (cargo,
  pnpm) stay in that group.
- Soft limit: `cache_hook::HOOK_LIMIT`, 120 s. It is the adapter's own deadline. The adapter stops its
  processes at it, restores or skips the folder, and reports. This is the normal path.
- Hard limit: the soft limit plus 10 s. At the hard limit the parent kills the worker's whole process
  group. Then a recovery worker (`recover`, 30 s) runs on the folder. The pnpm adapter puts its copy back.
  The cargo adapter has nothing to restore: its sweep removes nothing before the listing succeeds.
- If recovery fails or times out, core marks the folder damaged. A save skips a damaged folder with a
  line that says so. A prune of the folder that succeeds clears the mark. The mark file is
  `meta/line-locks/<line id>.damaged`.
- Root owner: never pruned.
- Environment of the worker: the owner's home, and `PATH` of the owner's tool folders. A process that
  is not root also keeps its own `PATH`. The root service never passes its own `PATH` on.

Locks:

- The hook holds the line lock (`meta/line-locks/<line id>`, flock, `Project::line_lock`) while it
  runs. Hooks of one line run one at a time. The line lock is reentrant in one process: a command
  that already holds it (a merge or a save of the line) takes it again without waiting. No other file
  under `meta/line-locks/` is flocked; `<line id>.damaged` is a plain file written atomically.
- The hook never holds the project (store) lock. Each call site drops the project lock before it
  calls the hook. The only call site is `save --caches` (the prune runs before its project lock).
- A cache overlay's own lock is taken only by the copy steps, never during a hook.

The hook runs only where a cache is saved. The caches that a line takes (`line new`, `merge`,
`merge --caches-only`, a follow pass) come from states that were pruned when they were saved. A
second prune at take time repeats the cargo listing (up to 120 s) for nothing. So they do not run it.

| Command | Hook | Next step |
| --- | --- | --- |
| `layr save --overlays` | in `prepare_caches`, for each cache folder of the line, after the cache is made a cache folder | the snapshot of the cache and the save |

A follow pass ends with `save --caches`, so it prunes the folders it took, through `prepare_caches`.
A union merge (`merge --caches-only`, or a merge) leaves the units of both sides in the target folder
until the next `save --caches` of that line. A cache that was saved before this design is pruned at
the next `save --caches` of its line.

`init` takes no caches (a new project has no folder overlays), so it has no call site.

Relocation is a second hook, `CacheAdapter::relocated(folder, at)`. It runs where a line takes a cache that
was built at another path, and `at` is the path where the folder is used from now on:

| Command | When |
| --- | --- |
| `layr line new` | after the project lock, when the line folder is in place (the owner cannot enter the store's work folder) |
| `layr merge`, `merge --caches-only`, a follow pass | in `pull_caches`, after each folder is in place |
| an automatic check (docs/checks.md) | on its copy, for the place where the check sees it |

The registry sources are the one path that relocation cannot change. Cargo keeps the absolute path of each
registry source (`<CARGO_HOME>/registry/src/...`) in the unit's fingerprint, so a cache built with another
CARGO_HOME builds every registry crate again; only the workspace crates stay warm. Use one absolute CARGO_HOME on
every machine that shares caches (the laptop shell, check runners, CI), outside any user home, for example
`/usr/local/share/layr-checks/<project>/cargo`. A check can read it read-only: an offline build writes nothing
there. When a cache came from a registry folder that this machine does not have, the relocation prints
`built with the cargo registry at <path>, which this machine does not have` (`CacheAdapter::relocation_note`).

The worker runs it as the owner, root too, also with `prune=none`, with a 60 s limit. It runs no process
and never fails a command. The cargo adapter reads the dep-info files (`<profile>/deps/*.d`). The first
rule of each names the place of its build. A unit built at another place goes stale when its dep-info
names a path of that place that rustc compiled in, or that cargo compares by its absolute path:

- `env-dep:CARGO_BIN_EXE_<bin>` (a test runs the binary of the other line);
- another `env-dep` with a path of the old place out of the old target folder (`CARGO_MANIFEST_DIR`);
- a source of the old place out of the old target folder (`clippy.toml`, `include_str!` of a file out of
  the package).

The adapter removes the fingerprint folder of such a unit, so the next build makes it again. A path in the
old target folder (`OUT_DIR`) does not make a unit stale: that content was copied with the folder, and
cargo finds it relative to the target folder. On the target folder of Studio (1,845 units) the pass takes
0.18 s.

### 18.6 Build env, profiles and the live set

The live set is the set of units that the `sweep` commands of `[workspace.metadata.layr-retest]` make
with the project env (`env`). `layr retest init` writes `env = { CARGO_INCREMENTAL = "1", RUSTC_WRAPPER = "" }`
into the manifest. A manifest without `env` gives the listing the default env of cargo: a build in the
shell with another env (for example `CARGO_INCREMENTAL=1` in release, where it is off by default) makes units
with other hashes, and those are not kept.

Each profile folder is pruned on its own. The commands of a profile (`--release` is `release`,
`--profile <name>` is that name, `dev` and `test` are `debug`, and the default is `debug`) list the units
of that folder only. A stale command of one profile keeps that folder. The other profiles are still pruned.
A profile folder that no command lists is not touched.

The listing never compiles. It runs cargo with a `RUSTC_WRAPPER` that refuses every compile (it passes
the version and target probes through). A unit that is not fresh stops the listing of its profile before any
build step. The wrapper is not part of a unit's hash.

`layr retest doctor` names each profile that is skipped: `prune skipped for <profile>: unit <unit> is not
fresh for the standard commands; build with `cargo <args>` or add it to [workspace.metadata.layr-retest]
sweep`.

### 18.7 Tests

- `src/cache_hook.rs` unit tests: a named adapter prunes and logs; `none` keeps the folder; an unknown
  name is logged; a failing adapter logs its reason; a slow adapter is cut off at the limit; the root
  owner is never pruned.
- `crates/layr-cache/tests/`: `cargo_prune.rs` (a stale unit is removed and the next build compiles
  nothing; a stale standard unit skips the folder; no sweep list skips), `pnpm_prune.rs` (a removed
  package is gone from `node_modules` and `.pnpm` with an address that refuses connections as the
  registry; a lockfile that needs a missing package and a stale lockfile skip the folder, which stays
  unchanged; no lifecycle script runs), `dispatch.rs`.
- `crates/layr-retest/tests/e2e.rs`: `layr retest prune target` runs the cargo adapter.
- `tests/t117-prune-hook.sh`: `line new` and `merge --caches-only` take the units without a prune; the next
  `save --caches` of the line prunes them; `prune=none`; an unknown adapter name; cargo missing for the owner;
  a root-owned line. Through the binary.
  Root (btrfs) on Linux, and macOS APFS, as the other `tests/t*.sh` files.

## 20. Source file times

Build tools compare file times: cargo builds a crate again when a source file is newer than its
build output. A line from a state, a merge and a follow pass write source files with the time of
the write (as `git checkout` does), so a warm cache looked stale and one or more crates compiled.

- A cache save (`save --caches`, the follow pass, a local merge, the save-back of a passing check and
  `save --overlays --save-back`) writes `.layr-times` into each
  cache folder before the snapshot: path, time, size and content hash (git blob id) of each tracked
  file of the working copy (the ignore rules keep it). It runs before the project lock.
- The file goes with the cache: snapshots, copies and overlay layers carry it, and it goes away
  with the cache state. No other record holds it.
- A line that takes caches (`line new`, `merge`, the follow pass, `merge --caches-only`) gives each
  tracked file whose size and content hash equal the recorded ones the recorded time back. A file
  with other bytes must look newer than the build output, so the tool builds it again: it keeps
  its time when that time is after the save of the cache (the time of `.layr-times`), and gets the
  current time otherwise. A line's files can carry an old time, because a clone keeps the time of
  the first write; a cache built later from other bytes then looked current, and cargo linked a
  stale crate.
- This is safe by content: the build output is current for the bytes it was built from, whatever
  the time. With equal bytes in two caches, the file gets the older time.
- Two caches can record other bytes for one file: a line saves only `target` after a change, and
  its `node_modules` keeps an older save. A line from it takes both. The file gets the time that
  the caches with its bytes recorded, when that time is after the save of each cache with other
  bytes: those see a newer file and build again, and the others stay warm. Otherwise the file
  counts as other bytes.
- Cost: one walk and one hash of the tracked files per save and per take (Studio, about 4,100
  files and 118 MB: measured in the PR). A take reads only files of the recorded size whose time
  differs, and the files that two caches recorded with other bytes.

## 21. Measurement 2026-11-11: a cache overlay in v2 objects

The question: what does a cache overlay cost in v2 objects ([objects](objects.md))? Tool: the
example `objects-bench` of `crates/layr-objects`, a release build, an Apple Silicon Mac (Lumina),
real `target` cache states of this repository.

| State | Previous state | Files | Logical size | Step | Time | Result |
| --- | --- | ---: | ---: | --- | ---: | ---: |
| `01a1b99e` | none | 25,828 (1,763 folders) | 11.33 GB | cold import, zstd level 3, all files | 9.2 s | 2.55 GB of loose objects |
| `01a1b99e` | none | 25,828 | 11.33 GB | repack, zstd level 19, blocks of 4 MiB | 99 s | 1.86 GB of packs |
| `01a1b9a8` (the same build) | `01a1b99e` | | | import | 1.06 s | 88 new objects, 86 KB |
| `01a1b9a9` (the same build) | `01a1b9a8` | | | import | 1.0 s | 3 new objects, 1.9 KB |
| `01a1ca61` (after a real rebuild) | `01a1b8bd` | 35,096 | 19.4 GB | import | 15.2 s | 84,390 new objects, 2.34 GB |

- Hot import (clones) failed in `objects-bench` only. The state files carry the immutable flag
  (`uchg`), and a clone keeps it. layr itself clears it: the project object store sets
  `clone_hook` (`sys::apfs_unseal_fd`, src/objstore.rs). The example tool sets no hook.
- Today each cache state is a full native folder. `du` reports about 15 GB for each; APFS clones
  share blocks, so `du` overstates the real use.

Result: one cache state with a new build costs about 2.3 to 2.6 GB compressed (1.9 GB packed). A
state with the same build costs kilobytes.
