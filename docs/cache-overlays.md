# Cache overlays and remotes per layer

<<<<<<< head
Status: design approved by the owner on 2026-10-27. Section 9 says which slice builds which part.
Built: slice a (section 11).
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

A cache is an ordinary overlay with properties. No separate cache concept remains in the model.

- An overlay has **properties**. Each property has a default that keeps the behaviour of section 9
  of [overlays](overlays.md):

  | Property | Values | Default | Meaning |
  | --- | --- | --- | --- |
  | `history` | `all`, `last`, `<N>` | `all` | How many states each overlay line keeps. `last` is `1`. Older states of the line are deleted at the next state of the line and by `gc`. |
  | `quiet` | on or off | off | The overlay files are not in `status`, `diff` and `add`. Their changes go into the overlay without the index (section 4). |
  | `merge` | `three-way`, `union-theirs` | `three-way` | How a merge of a line combines the follow overlay. `union-theirs`: the result has every file of both sides; on the same path the incoming line wins; no conflicts. |
  | `gc` | `none`, `lru:<size>` | `none` | `gc` removes files from the working folders of the overlay, least recently read first, until the overlay folder of each line is at most `<size>` (`500M`, `20G`). |
  | `folder` | a path | none | A **folder overlay**: the overlay is the whole folder at that path (section 3). |
  | `platform` | on or off | off | Each state records its platform key; a line takes a state only from its own platform (section 6). |

- A **cache overlay** is an overlay with `folder=<path>`, `quiet`, `history=last`,
  `merge=union-theirs`, `platform`, in follow mode. Its readers are the readers of the project.
- **`regenerable`** in `.layrattributes` is a short form: the folder that it marks gets a cache
  overlay with that folder and the defaults above, the first time `layr save --caches` saves it.
  `layr cache add <path>` does the same for the machine setting.
- Each line has its own overlay line in a follow overlay (`line-<line id>`). So **a line has
  caches**, not a state: `line new X <line>` takes the newest cache of that line.

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

A folder overlay is not part of the file stack of a line (`stack.<line id>`). Its files never
enter the base, the split of a named save or an assembled tree. The state that a line shows is
the head of the line's overlay line.

A folder overlay must be ignored by the ignore rules and must not hold files of the line's head
state, as for caches today (`design.md` section 5). A folder that holds versioned files stays a
plain folder; `save --caches` says so.

## 4. Commands

| Command | Effect |
| --- | --- |
| `layr overlay create <name> [--history all\|last\|<N>] [--quiet] [--merge three-way\|union-theirs] [--gc none\|lru:<size>] [--folder <path>] [--platform]` | Makes an overlay with these properties. |
| `layr overlay set <name> [--history ...] [--quiet\|--no-quiet] [--merge ...] [--gc ...] [--platform\|--no-platform]` | Changes properties. The owner only. A record `overlay.set` keeps the change. `--folder` cannot change. |
| `layr overlay list`, `layr overlay show <name> --json` | Also show the properties. |
| `layr save --caches` | For each cache path of the line (as today: `regenerable` and `cache add`): makes its cache overlay when missing, makes the folder a nested layer when a tool made a plain folder, and saves a new state of the overlay line of this line (one clone). Older states go by `history`. |
| `layr save -m <message>` | Also saves the quiet file overlays of the stack from the working folder (section 4.1). |
| `layr line new <name> <line>` | The new line takes the newest state of each cache overlay of the source line. |
| `layr line new <name> <state>` | The new line takes, for each cache overlay, the newest state saved at that state or at its nearest ancestor (the rule of `warm_cache` today). It prints `no cache for <path>` when there is none. |
| `layr merge <line>` | Merges each follow overlay of the source line, with the overlay's `merge` property. A cache overlay unions the files into the target's folder. |
| `layr cache pull <line or state>` | Takes the newest caches of another line or state into the current line (`merge=union-theirs` for one overlay, without a merge of the shared set). |
| `layr cache list` | The cache overlays of the line: path, state, size on disk. |
| `layr gc` | Also applies `history` to overlay lines and `gc lru:<size>` to working folders. |
| `layr overlay remote add <overlay> <git URL>` | The overlay's history goes to its own git repository (section 7). Refused for a cache overlay. |
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

`<hash>` is the first 16 hex digits of the SHA-256 of the files that the setting
`overlay.<name>.key-files` lists, from the line's working folder at the save (for example
`["Cargo.lock", "rust-toolchain.toml"]`). layr does not know toolchains; the program on top names
the files that select a toolchain. A missing file counts as empty.

- `line new`, `cache pull` and merges take a state only when its key equals the key of the target
  line (computed in that line at that time).
- Remotes (section 7): a layr remote carries the states of every platform; a receiving machine
  imports only the states of its own `<os>-<arch>`.

## 7. Remotes per layer

There are two kinds of remote:

- A **layr remote** (a store: `file://`, `s3://`, `layr+https://`; or a peer: `ssh://`) takes the
  whole repository: the shared set and every overlay, each overlay encrypted for its readers, as
  [overlay sync](overlay-sync-design.md) decides. Cache overlays travel too. A receiving machine
  imports a cache state only for its own platform.
- A **git remote** takes one layer. The shared set goes to the project's git remotes (`origin`),
  as today. An overlay goes to its own git repository: `layr overlay remote add <overlay> <url>`
  records `overlay.<name>.git.<remote>` in the overlay's own records, so only its readers see the
  URL. The branch of an overlay line is the name of the line that uses it. A cache overlay never
  goes to git: `overlay remote add` refuses it.

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

- **Stores with old caches.** `StateRec.caches` stays readable. `line new <state>` and `try` look
  for a cache overlay state first and then for an old cache of the state or an ancestor, with
  the rules of today (protected line or same user; platform). `gc` keeps old cache snapshots by
  `retention.caches` as today, until no state has one.
- **`save --caches`** keeps its name and meaning for the user: it saves the caches of the line.
  It writes cache overlay states and no more `StateRec.caches`. On a protected line with no
  changes it no longer needs to give the caches to the head state: the caches belong to the line.
- **`regenerable` and `cache add|remove|list|repair`** keep their meaning. `cache repair` turns a
  plain folder at a cache path into a nested layer, as today.
- **`sync --caches`** (live move) carries cache overlays as it carries other overlays; the old
  flag keeps the old cache snapshots for one release.
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

1. **Unreviewed build output in protected lines.** `merge=union-theirs` makes the cache of a
   merged line part of the cache of `main`. Today a cache reaches other users only when its maker
   saved it on a protected line. With this design the merger of a line also passes the line's
   build output on. Proposal: keep the merge, and let `line new` of another user take a cache of
   a protected line only (as today).
2. **Readers of cache overlays.** Proposal: the readers of the project. A private cache needs
   `overlay create --readers` by hand.
3. **Toolchain in the platform key.** layr hashes the files that `key-files` names. A toolchain
   that no file names (a system compiler) is not in the key.
4. **LRU on APFS.** macOS updates access times lazily; `lru` uses the access time and the
   modification time, whichever is newer.

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
||||||| base
=======
Status: design approved by the owner on 2026-10-27. Section 8 says which slice builds which part.
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

A cache is an ordinary overlay with properties. No separate cache concept remains in the model.

- An overlay has **properties**. Each property has a default that keeps the behaviour of section 9
  of [overlays](overlays.md):

  | Property | Values | Default | Meaning |
  | --- | --- | --- | --- |
  | `history` | `all`, `last`, `<N>` | `all` | How many states each overlay line keeps. `last` is `1`. Older states of the line are deleted at the next state of the line and by `gc`. |
  | `quiet` | on or off | off | The overlay files are not in `status`, `diff` and `add`. Their changes go into the overlay without the index (section 4). |
  | `merge` | `three-way`, `union-theirs` | `three-way` | How a merge of a line combines the follow overlay. `union-theirs`: the result has every file of both sides; on the same path the incoming line wins; no conflicts. |
  | `gc` | `none`, `lru:<size>` | `none` | `gc` removes files from the working folders of the overlay, least recently read first, until the overlay folder of each line is at most `<size>` (`500M`, `20G`). |
  | `folder` | a path | none | A **folder overlay**: the overlay is the whole folder at that path (section 3). |
  | `platform` | on or off | off | Each state records its platform key; a line takes a state only from its own platform (section 6). |

- A **cache overlay** is an overlay with `folder=<path>`, `quiet`, `history=last`,
  `merge=union-theirs`, `platform`, in follow mode. Its readers are the readers of the project.
- **`regenerable`** in `.layrattributes` is a short form: the folder that it marks gets a cache
  overlay with that folder and the defaults above, the first time `layr save --caches` saves it.
  `layr cache add <path>` does the same for the machine setting.
- Each line has its own overlay line in a follow overlay (`line-<line id>`). So **a line has
  caches**, not a state: `line new X <line>` takes the newest cache of that line.

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

A folder overlay is not part of the file stack of a line (`stack.<line id>`). Its files never
enter the base, the split of a named save or an assembled tree. The state that a line shows is
the head of the line's overlay line.

A folder overlay must be ignored by the ignore rules and must not hold files of the line's head
state, as for caches today (`design.md` section 5). A folder that holds versioned files stays a
plain folder; `save --caches` says so.

## 4. Commands

| Command | Effect |
| --- | --- |
| `layr overlay create <name> [--history all\|last\|<N>] [--quiet] [--merge three-way\|union-theirs] [--gc none\|lru:<size>] [--folder <path>] [--platform]` | Makes an overlay with these properties. |
| `layr overlay set <name> [--history ...] [--quiet\|--no-quiet] [--merge ...] [--gc ...] [--platform\|--no-platform]` | Changes properties. The owner only. A record `overlay.set` keeps the change. `--folder` cannot change. |
| `layr overlay list`, `layr overlay show <name> --json` | Also show the properties. |
| `layr save --caches` | For each cache path of the line (as today: `regenerable` and `cache add`): makes its cache overlay when missing, makes the folder a nested layer when a tool made a plain folder, and saves a new state of the overlay line of this line (one clone). Older states go by `history`. |
| `layr save -m <message>` | Also saves the quiet file overlays of the stack from the working folder (section 4.1). |
| `layr line new <name> <line>` | The new line takes the newest state of each cache overlay of the source line. |
| `layr line new <name> <state>` | The new line takes, for each cache overlay, the newest state saved at that state or at its nearest ancestor (the rule of `warm_cache` today). It prints `no cache for <path>` when there is none. |
| `layr merge <line>` | Merges each follow overlay of the source line, with the overlay's `merge` property. A cache overlay unions the files into the target's folder. |
| `layr cache pull <line or state>` | Takes the newest caches of another line or state into the current line (`merge=union-theirs` for one overlay, without a merge of the shared set). |
| `layr cache list` | The cache overlays of the line: path, state, size on disk. |
| `layr gc` | Also applies `history` to overlay lines and `gc lru:<size>` to working folders. |
| `layr overlay remote add <overlay> <git URL>` | The overlay's history goes to its own git repository (section 7). Refused for a cache overlay. |
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

`<hash>` is the first 16 hex digits of the SHA-256 of the files that the setting
`overlay.<name>.key-files` lists, from the line's working folder at the save (for example
`["Cargo.lock", "rust-toolchain.toml"]`). layr does not know toolchains; the program on top names
the files that select a toolchain. A missing file counts as empty.

- `line new`, `cache pull` and merges take a state only when its key equals the key of the target
  line (computed in that line at that time).
- Remotes (section 7): a layr remote carries the states of every platform; a receiving machine
  imports only the states of its own `<os>-<arch>`.

## 7. Remotes per layer

There are two kinds of remote:

- A **layr remote** (a store: `file://`, `s3://`, `layr+https://`; or a peer: `ssh://`) takes the
  whole repository: the shared set and every overlay, each overlay encrypted for its readers, as
  [overlay sync](overlay-sync-design.md) decides. Cache overlays travel too. A receiving machine
  imports a cache state only for its own platform.
- A **git remote** takes one layer. The shared set goes to the project's git remotes (`origin`),
  as today. An overlay goes to its own git repository: `layr overlay remote add <overlay> <url>`
  records `overlay.<name>.git.<remote>` in the overlay's own records, so only its readers see the
  URL. The branch of an overlay line is the name of the line that uses it. A cache overlay never
  goes to git: `overlay remote add` refuses it.

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

- **Stores with old caches.** `StateRec.caches` stays readable. `line new <state>` and `try` look
  for a cache overlay state first and then for an old cache of the state or an ancestor, with
  the rules of today (protected line or same user; platform). `gc` keeps old cache snapshots by
  `retention.caches` as today, until no state has one.
- **`save --caches`** keeps its name and meaning for the user: it saves the caches of the line.
  It writes cache overlay states and no more `StateRec.caches`. On a protected line with no
  changes it no longer needs to give the caches to the head state: the caches belong to the line.
- **`regenerable` and `cache add|remove|list|repair`** keep their meaning. `cache repair` turns a
  plain folder at a cache path into a nested layer, as today.
- **`sync --caches`** (live move) carries cache overlays as it carries other overlays; the old
  flag keeps the old cache snapshots for one release.
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

1. **Unreviewed build output in protected lines.** `merge=union-theirs` makes the cache of a
   merged line part of the cache of `main`. Today a cache reaches other users only when its maker
   saved it on a protected line. With this design the merger of a line also passes the line's
   build output on. Proposal: keep the merge, and let `line new` of another user take a cache of
   a protected line only (as today).
2. **Readers of cache overlays.** Proposal: the readers of the project. A private cache needs
   `overlay create --readers` by hand.
3. **Toolchain in the platform key.** layr hashes the files that `key-files` names. A toolchain
   that no file names (a system compiler) is not in the key.
4. **LRU on APFS.** macOS updates access times lazily; `lru` uses the access time and the
   modification time, whichever is newer.
>>>>>>> 68562f4f9aad737f8ddf01a17d84ef60
