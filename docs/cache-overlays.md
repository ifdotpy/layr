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
  `merge=union-theirs`, `platform`, in follow mode. Its readers are its owner and the machines
  that the project trusts (owner decision 2026-10-27), so store copies and peer sync give caches
  only to those machines.
- **`regenerable`** in `.layrattributes` is a short form: the folder that it marks gets a cache
  overlay with that folder and the defaults above, the first time `layr save --caches` saves it.
  The project setting `regenerable` (`layr config regenerable '["<path>"]'`) names cache paths for
  every line; a new line gets an empty cache of such a path at once.
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
| `layr save --caches` | For each cache path of the line (`regenerable`, in `.layrattributes` or the project setting): makes its cache overlay when missing, makes the folder a nested layer when a tool made a plain folder, and saves a new state of the overlay line of this line (one clone). Older states go by `history`. |
| `layr save -m <message>` | Also saves the quiet file overlays of the stack from the working folder (section 4.1). |
| `layr line new <name> <line>` | The new line takes the newest state of each cache overlay of the source line. |
| `layr line new <name> <state>` | The new line takes, for each cache overlay, the newest state saved at that state or at its nearest ancestor (the rule of `warm_cache` today). It prints `no cache for <path>` when there is none. |
| `layr merge <line>` | Merges each follow overlay of the source line, with the overlay's `merge` property. A cache overlay unions the files into the target's folder. |
| `layr overlay repair <overlay>` | In the current line, the folder of that folder overlay: a plain folder becomes a cache again; a cache with versioned files becomes a plain folder. |
| `layr overlay list` | Also shows, for each folder overlay in a line, the state of its folder, its size on disk and its saved state (was `layr cache list -v`). |
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

`<hash>` is the first 16 hex digits of the SHA-256 of the files that the setting
`overlay.<name>.key-files` lists, from the line's working folder at the save (for example
`["Cargo.lock", "rust-toolchain.toml"]`). layr does not know toolchains; the program on top names
the files that select a toolchain. A missing file counts as empty.

- `line new` and merges take a state only when its key equals the key of the target
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

- **Stores with old caches.** `StateRec.caches` stays readable. `line new <state>` and `try` look
  for a cache overlay state first and then for an old cache of the state or an ancestor, with
  the rules of today (protected line or same user; platform). `gc` keeps old cache snapshots by
  `retention.caches` as today, until no state has one.
- **`save --caches`** keeps its name and meaning for the user: it saves the caches of the line.
  It writes cache overlay states and no more `StateRec.caches`. On a protected line with no
  changes it no longer needs to give the caches to the head state: the caches belong to the line.
- **`regenerable`** keeps its meaning. The `cache` commands are removed (section 15). `overlay repair`
  turns a plain folder at a cache path into a nested layer, as `cache repair` did.
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

## 12. Built: slice b

Built on 2026-10-27: folder overlays and caches as cache overlays. Tests:
`tests/t59-cache-overlays.sh` (one user; macOS and Linux) and unit tests in
`src/cache_overlay.rs`. The model is in `src/cache_overlay.rs`.

- `overlay create <name> --folder <path>` makes a folder overlay (follow mode, not hidden; one
  overlay per folder). `overlay list` shows `folder=<path>`. `overlay use` and `overlay move`
  refuse a folder overlay.
- `save --caches` makes the cache overlay `cache.<folder>` (`/` as `.`) of each cache path when it
  is missing: folder overlay, readers: the trusted machines, `history=last`, `quiet`, `merge=union-theirs`, and the
  setting `overlay.<name>.cache = true`. It then saves one snapshot of the folder as a state of the
  line's overlay line. Each state records the user, the platform (`<os>-<arch>`) and whether the
  line is protected, with the rules of `design.md` section 5 for who may take it.
- `line new X <line>` takes the newest cache state of that line (dogfood item 10).
  `line new X <state>` takes the newest cache of the line that made the state, or of the line of
  its nearest ancestor that has one (item 21). A cache overlay goes into a new line only while
  its folder is a cache path there (`regenerable`) and the ignore rules ignore it.
  Without a cache overlay state, the old caches of the state apply as before.
- `layr merge <line>` then brings the merged line's caches into the target (section 5): a
  fast-forward of the target's overlay line, or a `union-theirs` state; the incoming files are
  copied over the target's folder; an empty folder gets one snapshot. A failure here is a
  warning; the merge stays. Into a protected line only when the merge met its protection (each
  required check passed on the new head, the required approvals of the merged state); otherwise
  the merge prints `Caches of '<line>' not merged`.
- Cache overlay readers: the owner and every machine the project trusts (`machine:<UUID>`), not
  `*`. Local users take caches by the rules of `design.md` section 5 (their own, or saved on a
  protected line).
- `layr cache pull <line or state>` (removed in section 15): the same for one line, without a merge (item 24).
- `layr cache list -v` (removed in section 15) showed the size of each cache folder and the overlay state
  the line saved; `overlay list` shows both.
- `gc` applies `gc=lru:<size>` to the folder of each line (the newer of access and change time;
  apparent sizes) and deletes the cache states of deleted lines.
- `line delete` also deletes the cache overlay lines (`line-<id>`) of the line. `undo` of that delete
  restores the line but not its cache overlay lines: the next `save --caches` makes new ones. `gc`
  and `fsck --repair` remove cache overlay lines of deleted lines that older versions left behind.
  `fsck` marks such a problem "(of a deleted line)".
- The secret detector does not read folder overlays: they hold build output.

Limits of slice b:

- `save --caches` still also writes the old caches of a state (`StateRec.caches`), so `try`, the
  live move (`sync --caches`) and older binaries keep working. That is two snapshots per folder.
  Slice d moves the live move to cache overlays; then the old write can go.
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
- The live move (`sync --caches`) still uses the old state caches.

## 15. Command change (owner decision, 2026-10-27)

A cache is an overlay. The `layr cache` command group is removed. Each form fails with exit
status 2 and names its replacement:

| Removed form | Replacement | Message |
| --- | --- | --- |
| `layr cache list [-v]` | `layr overlay list` | `layr cache list was removed. Use 'layr overlay list'.` |
| `layr cache add <path>` | `regenerable` in `.layrattributes`, then `layr save --caches` | `layr cache add was removed. Use 'regenerable' in .layrattributes, then 'layr save --caches'.` |
| `layr cache remove <path>` | `-regenerable` in `.layrattributes`, or `layr config regenerable` without the path | `layr cache remove was removed. Use '-regenerable' in .layrattributes, or 'layr config regenerable' without the path.` |
| `layr cache repair` | `layr overlay repair <overlay>` | `layr cache repair was removed. Use 'layr overlay repair <overlay>'.` |
| `layr cache pull <line or state>` | `layr line new <name> <line>`, or `layr merge <line>` | `layr cache pull was removed. Use 'layr line new <name> <line>' or 'layr merge <line>'.` |

A bare `layr cache`, or any other form, fails with `layr cache was removed. Use 'layr overlay list'.`
The lines that carry caches are unchanged:
`line new` takes them, and `merge` brings them back.

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
