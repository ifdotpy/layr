# Command reference

This page is part of the [Layr documentation](../README.md#documentation).

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status [--json] [--no-overlaps] [--no-ignored]`, `diff [-z] [--exclude-standard]`, `add`, `rm`, `mv`, `apply [--cached]`, `save -m <message> [-a] [--amend] [--json]`, `save [--note <text>] [--keep <name>] [--json]`, `keep [list [--json] \| drop <name>... \| drop --all]`, `restore [--clean]`, `reset`, `park`, `clean` |
| history       | `log [--json]`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `line new <name> [<rev>] [--count N] [--json] [--view virtual]`, `line show <line> --none\|--virtual\|--folder [<path>]`, `line list [--json]`, `line delete`, `line rename`, `line move`, `tag`, `merge [--expect]`, `pick`, `invert`, `replay`, `adopt` |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `attest <kind> <rev> [--payload <file>]`, `attestations <rev> [--kind <kind>] [--json]` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `try <state> --check <name>`, `bisect list [--json]`, `bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] [--json] -- <cmd>`, `gc [-n] [--repair] [--dedup]`, `du [<line>...] [--json]`, `du --store [--json]`, `du --volumes [--json]`, `fsck`, `reindex` |
| Git bridge    | `git remote`, `git fetch`, `git pull`, `git push [--squash] [--include-base <lines>]`, `git clone`, `git sync`                             |
| remotes       | `remote add\|list\|self\|remove\|trust`, `remote public <remote> on\|off`, `sync <remote>`, `push <remote>`, `push [--dry-run]` (every layer to each of its remotes), `overlay remote add\|list\|remove`, `fetch <remote> [--line <name>,...] [--overlay <layer>,...] [--heads [--only-line <line>]]`, `pull <remote>`, `clone <url> [--line <line>,...] [--overlay <layer>,...] [--heads [--only-line <line>]]` (`--line` on clone takes only the caches of those lines; `--overlay` only those layers), `login <remote>` (Layr Cloud) |
| OCI           | `push <oci-remote> <rev> [--runnable <name>] [--json]`, `access token <principal> [--until <duration>]`, `access token --revoke <principal>` |
| records       | `op export`, `op import` (JSONL) |
| copies        | `export <folder>`                                                                              |
| views         | `view [<rev or line>] [--overlay <overlay>,...] [--folder <dir>] [--json]` (`--overlay` makes another view of the line or state with those overlays; `--folder` writes the files of the view to a new folder; see overlays.md section 1a, slice 5), `view save [<line>] [--json]`, `view show <id> [--json]` |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow] [--hidden] [--folder <path>] [--history all\|last\|<N>] [--quiet] [--merge three-way\|union-theirs] [--gc none\|lru:<size>] [--platform <key>\|any] [--trust protected\|readers]`, `overlay set <name> <properties>`, `overlay list [--json] [--size]`, `overlay show-to-project <name>`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>` (changes the view of the current working folder), `overlay order [<name>,...]` (shows or sets the project order of overlays, the order of the default view of a new line folder), `overlay move [--purge] <name> <paths>`, `overlay repair <overlay>`, `overlay remove <cache overlay>` (removes a cache overlay from the project; see cache-overlays.md section 18), `overlay log\|show\|diff\|grep\|ls-files <name>`, `overlay migrate-parts [-n] [-q]` (gives the head of each line its parts; see overlays.md section 1a) |
| refs          | `ref set <name> <rev> [--expect <rev\|none>] [--ttl <dur>]`, `ref list [<prefix>]`, `ref log <name>`, `ref delete <name> [--expect <rev>]` |
| queue         | `queue add <rev> [--into <line>] [-m <conventional title>] [--json]`, `queue list [--into <line>] [--json]`, `queue remove <entry>`, `queue run [--into <line>] [--push <remote>]` |
| follow        | `follow <remote>/<branch>`, `follow --run`, `follow --tick`, `follow`, `follow --remove` ([layr follow](follow.md)) |
| tests         | `retest run [--full] [--save] [--tool vitest\|jest\|pytest] [-- <nextest args>]`, `retest init [--yes]`, `retest doctor` (see `docs/retest.md`) |
| probes        | `probe run <rev> [--probe <name>] [--json]`, `probe diff <base> <head> [--json]` |
| changes       | `change list [--all\|--closed] [--json]`, `change show <change> [--json]`, `change log <change>`, `change diff <change> [--from vN\|--since-review [--person <login>]] [--to vM]`, `change edit <change> [--title <title>] [--description <text>\|--description-file <file>]`, `change close <change> [-m <reason>]`, `change reopen <change>`, `change numbering [--hand-to <machine>]`. `<change>` is `#12`, `12`, a change id or the start of one ([review](review.md#change-numbers-titles-and-closing)) |
| review        | `comment <change> (-m <text>\|<text>) [--file <path> --line <a>[-<b>]] [--state <rev>\|--version vN] [--reply-to <id>] [--json]`, `comment reply <comment> -m <text> [--json]`, `comment resolve\|unresolve <comment> [--json]`, `react <comment or change> <name> [--remove] [--json]`, `review request\|cancel <change> <reviewer>... [--json]` (a reviewer is `person:<login>`, `user:<name or uid>`, a user name, or `machine:<name or id>`; see [review](review.md#review-threads-reactions-and-reviewers)) |
| requests      | `request <kind> ... --person [--remote <name>] [--expires <epoch-ms>]`, `requests apply [--remote <name>] [--json]` |
| person        | `person keys [--paper-key-out <path>] [--site <url>] [--store <url>]`, `person show [--json]`, `person accept`, `access person-key <login> <public-key>` |
| setup         | `init [--allow-root-snapshots] [--volume <name>]`, `init [<project>] --from-git <checkout> [--line <name>]`, `projects`, `project delete <project>` (a project without lines, admin only), `config`, `daemon [--allow-root-snapshots]`                             |
| volumes       | `volume add <name> <path>`, `volume list [--json]`, `volume remove <name>`, `volume default <name>`, `project move <project> <volume>`, `clone <url> [<project>] --volume <name>`, `git clone <url> [<project>] --volume <name>` ([volumes](volumes.md)) |
| store         | `store remove <path> [--force] [-q]` (deletes a whole store folder; see [Remove a store](#remove-a-store)), `store-version [--json]` ([store version](local-store-version.md)) |
| JSON schemas  | `schema` (lists the schemas), `schema <name>` (prints one; see [JSON output](json.md)) |

### Command line

- Global options come before the command: `-C <folder>`, `--root <store>`, `--project <name>`,
  `--line <name>`. `--line` alone names a line of the project of the current folder:
  `layr --line topic-1 log` is the same as `layr log topic-1`. `layr --line a log b` with two
  different lines is refused.
- `--` ends the options. No argument after it is an option. Use it for text that starts with `-`:
  `layr comment <change> -- "-1 for this"`. Without `--`, such text is refused as an unknown
  option, and the error tells you to put `--` before it.
- Some commands give `--` a second meaning: paths (`diff`, `log`, `show`, `blame`, `grep`,
  `reset`) or a command to run (`try`, `bisect run`, `fetch --command`).
- An option that takes a value takes the next argument, also when it starts with `-`:
  `layr save -m "-x"`, `layr save -m "-- looks good"`.
- `layr <command> --help` (or `-h`) and `layr help <command>` print the usage of that command.
  `layr help <command> <subcommand>` prints only that subcommand, for example `layr help line move`.
  A command without its own usage text prints the lines of the general help that name it.
  `layr help` and `layr --help` print the general help.
- `-h` and `--help` after `--` are no request for help. Also not as the value of a text option
  such as `-m`, `--message` or `--note`. `grep` keeps `-h` for its own use: use `layr grep --help`.

`init` and `daemon` refuse a store in the root subvolume, because a system snapshot rollback would empty it. `--allow-root-snapshots` or `LAYR_ALLOW_ROOT_SNAPSHOTS=1` overrides the check. See [Btrfs root snapshots](btrfs-root-snapshots.md).

`init [<project>] --from-git <checkout>` makes a project from a Git checkout, in place. The
history comes from the folder, with no network: the local branches resolve as `local/<branch>`
and the branches of its `origin` as `origin/<branch>` (imported on first use). The project's
`origin` is the checkout's. The line of the current branch (or `--line`; `main` for a detached
HEAD) starts at HEAD. If local file or directory modes differ from Git's modes, a child state
preserves them. Its parent is the imported HEAD. Ignored `target` folders next to a `Cargo.toml` and ignored `node_modules`
folders next to a `package.json` (at most three levels deep) move into the line and become its
caches (the project setting `regenerable`), so nothing is built again. Then the checkout is kept
as `<checkout>.before-layr`, and the line moves to the checkout's path (`line move`). For a
`Cargo.toml` at the top it prints how to run `layr retest init` in a line of its own: the setup
changes files of the project, and the new line is protected. The command refuses a checkout with
uncommitted changes, one with more worktrees, a subfolder of a checkout, and a checkout on
another file system than the store. The project name is the folder name unless given.

layr ignores what git ignores: the `.gitignore` and `.layrignore` files of the tree, and the
local file `.git/info/exclude` of a line folder (below the root's rule files, as in git; it never
goes into a state). `init --from-git` adds the patterns of the user's `core.excludesFile` (or
`~/.config/git/ignore`) to the checkout's `.git/info/exclude` first, so the import takes no file
that `git status` did not show, and keeps all local patterns as the project setting `ignore.local`.
The `.git` folder of each line of the project gets them in its `info/exclude`.

Person signing-key pins are machine-local: enrollment pins them on the executor, and an admin can later run `layr access person-key <login> <public-key>` there.
Pins do not replicate because a trusted writer machine must not replace a person's key and impersonate their approvals.

Successful peer `push` and `sync`, and store `push`, append a signed operation record.
The record includes the remote name and URL, direction, state IDs, counts, bytes, overlays, and result.
The parent record contains only parent state IDs and counts.
Each overlay has a separate transfer record in its own log.
The URL excludes credentials, query parameters, and fragments.
The record stays local until the next exchange sends it.
OCI push exports an artifact and writes no transfer record.
`op log` shows these project operations from any line.
`remote list --json` includes `copies` with the last success time, machine, and transfer details.

Public store projects use the same encrypted objects and reader slots as private stores:

```sh
layr remote public archive on
layr clone file:///path/to/store copy --public
```

`remote public <remote> on` requires a current project admin and this machine's existing
writer slot. The signed project records in the store must trust the machine's key.
Push the project before publishing it. Type the local project name at the confirmation prompt.
All current and future project data becomes readable to anyone with access to the store objects.
The command creates `public.json`, signed by the writer machine, and `slots/public.age` once.
The manifest publishes a public opener identity. The vault identity stays inside sealed slots
and in reader memory. Repeating the command preserves both published objects.
An interrupted publication can be completed by running the command again.

`remote public <remote> off` rotates to a private epoch. Earlier data remains readable
to anyone who already had access. `remote mode <remote> private|service|public` selects
an access mode; closing access rotates, and opening access adds an ordinary reader slot.
`remote rotate <remote> [--remove <principal>...] [--reason <text>]` starts a new epoch
for the remaining readers. See [access modes and epochs](access-modes.md).

`clone --public` uses only public access. Without the flag, clone, fetch and mount use public
access when no personal, machine or paper slot opens. Public clones create no machine slot.
Their local `cloud.remote.<name>.public` setting permits fetch with normal project read access.
Granting the machine its own valid slot clears that setting at the next fetch; subsequent
operations use the ordinary remote policy. Private remotes and all pushes keep their policy.
A public-only reader can inspect lines, states and files, diff them and build a local copy.
Push and remote key grants require private writer access and are refused for public-only readers.
A public mount does not save the decrypted vault identity to the Keychain.
Overlays keep their own reader slots and default to private. Only an explicit
`overlay mode <name> service|public` opens a visible overlay, within the project mode.
A hidden overlay leaves no trace in the public reader's project or command output.

Anonymous gateway reads for `layr+https://host/project` use these GET routes:

- `/p/project/store.json`, `/p/project/public.json`, `/p/project/slots/public.age`.
- `/p/project/records/<machine>/<first>-<last>.age` and `/p/project/states/<state>/<attempt>.tree.age` or `.../<attempt>.<part>.data.age`
  (the existing record and state object names).
- `/p/project/objects?prefix=records/` and `/p/project/objects?prefix=states/`.
  Listings return `{"objects":[{"key":"<relative object name>","modified":<epoch milliseconds>}]}`.
  More specific prefixes below `records/` or `states/` use the same route.

These requests carry no token or read proof. Only record and state prefixes can be listed.
Overlay and people prefixes are excluded. Public requests never use the authenticated
`/v1/p/project/objects/` routes for object reads and cannot write, collect, or issue tokens.
The gateway must expose the routes only when a signed public manifest is present.

`line new <name> [<rev>] [--count N]` creates lines. A line starts at the state that `<rev>` names when the command reads it; a save or a fetch that moves that line during the command does not change the start. Use `line list`, `line delete <name>`, or `line rename [<old>] <new>` to manage them.
`line delete <name>` refuses a line with saves that are not merged into the current line; `-D`
deletes it anyway. It also refuses a line whose folder has unsaved changes: modified, added or
deleted files, and new files that are not ignored. `-D` does not change this. An automatic save
(`layr save` without `-m`) does not count as saved. The error names the line and its files. Save
the changes in a named state (`layr add`, then `layr save -m`), park them (`layr park`), or add
`--discard` to delete the line with them. `-f` is `-D` and `--discard` together. A discarded line
stays in the `deleted` ring, so `layr undo` brings it back with its changes.
With `--count N`, names run from `<name>-1` through `<name>-N`. The main project's line records
commit together in one durable transaction: failure records none of the batch and removes its
published folders. If a commit reports an error after it succeeded, all recorded lines and their
folders are kept and the error names them. Overlay preparation records belong to separate overlay
projects and may remain after a failed batch; they are not part of this transaction.
A line is a folder. Use `cd` to enter it.
`line rename [<old>] <new>` gives the line a new name and keeps its folder where it is. A shell,
an editor or an agent that works in the folder keeps a valid working folder. The line has the new
name everywhere: in the records, on its overlays, in `line list` and in `status`. `line list
--json` shows the folder path, which can differ from the name. A rename of a line in the store
keeps `lines/<old>`; a rename back to `<old>` makes it the default folder again. Run
`line move <new>` to move the folder to `lines/<new>`. `undo` of a rename also keeps the folder.
`line new <old>` refuses while the kept folder is there. The error names the line that owns the
folder and the `line move` command that frees the name.
`line move <line> <path>` keeps the working folder of a line at `<path>`, outside the store, for
example `~/Projects/app` as the real folder of the line `main`. `line move <line>` without a path
moves it back to `lines/<name>`, also from a folder that `line rename` kept. The path must not exist, its parent must exist and be writable
by you, it must be outside the store and on the same file system as the store, and it must not
overlap the folder of another moved line. You need the right to manage the line, and you cannot
move the line of the current folder. The move is one rename under the project lock, so `.git`,
caches and overlays move with the folder. Commands in the moved folder and its subfolders find
their line as in the store. `line list --json` and `--path`, `du`, `du --store`, `gc` and
`line delete` use the real path. `line rename` keeps the folder where it is.
The path is local to this machine and is never replicated.
`line new` refuses an automatic state that holds unsaved work (paths that are not ignored and
differ from both its parent and the current head of its line): the new line would carry that work
into its saves and pushes. An automatic state with only
ignored build output is a warm start.

Files that `restore`, `reset`, `merge`, `pick`, `undo`, `apply` and `park` write into the working
folder get the current time, as `git checkout` gives them, so build tools that compare times
(Cargo, make) rebuild them. Files that stay keep their time.

`reset <rev>` without `--hard` moves the head and keeps the files (mixed, as in git). It lists the
files that now differ from the new head and names `layr reset --hard <rev>`, which changes them.

Protection (`layr protect`) is about the history of a line: who can move its head. It does not
lock the working folder. In a protected line such as `main`, the owner can use `restore`,
`clean` and `reset` to the current head (`reset`, `reset --hard`, `reset --hard head`) to undo
changes in the working folder. A `save`, and a `reset` to any other state, still refuse.

`layr undo` reverses the newest operation of the line, an automatic save too. A second `layr undo`
reverses that undo. Name an operation id to reverse an older one. Undo of an automatic save puts the
working folder back to the state the save changed from, keeps the head and the stage, and keeps the
paths that later work changed. Automatic saves that record no such state (older layr versions, cache
and staging saves) are skipped by `undo` with no id.
`pick` on a line with uncommitted work prints the kept files: `kept N uncommitted file(s):`
and up to 20 names, then `and N more`.

`save -m <message>` makes a named state. `--message` is the long form of `-m`.
`save -F <file>` reads the message from a file. `--file` is the long form of `-F`.
`save --amend` replaces the current head state. Without a new message, it keeps the current message.
The signed operation records its actor, version kind, source state, target line, and target head.
`replay` and `pick` record the same version fields. Old states need no migration.

After a named save makes a state, `save` prints on stderr the files that it left out. It prints
nothing when none are left out. `-q` does not hide these lines.

- `not saved (untracked): <paths>` names the files that are neither ignored nor in the new state.
  A new folder that holds only untracked files is named once, as `dir/`.
- `not saved (modified, not added): <paths>` names tracked files whose working content differs
  from the new state. `save -a` stages all tracked changes, so this line normally appears only
  for `save -m` without `-a`.

Each line shows at most 20 paths, then `and N more`. Ignored files, quiet overlay paths and cache
overlay folders are never named. Automatic saves print no such lines. To save the left out files,
use `layr add <path>` and save again.

Named saves also accept these options:

- `-a` or `--all` includes changes to tracked files.
- `--author <author>` sets the author.
- `--allow-empty` permits a state without file changes.
- `--allow-conflict-markers` permits a changed file with a full set of conflict markers ([merge](merge.md#conflicts-and-conflict-markers)).
- `-q` or `--quiet` hides the usual output.
- Other accepted options: `--no-edit`, `-s` or `--signoff`, `--no-verify`, `-v` or `--verbose`.

`save` without `-m`, `-F`, or `--amend` makes an auto state.
Use `--note <text>` to add an optional note to the auto state.
If nothing changed since the newest auto state made over the head (or since the head when there is none), no state is made. The output names the existing state: `<line> <state id>`.
Use `--running`, `--force`, or `-q` with an auto save.

A named save also saves the build-output overlays (docs/overlays.md, section 1a, slice 6). A
`regenerable` folder becomes the overlay of this platform (`target.macos-aarch64`) at its first
named save. A folder that changed since the line's part gets a new part; an unchanged folder keeps
its part. The new state names the part (`parts`), so `line new <state>` takes it.

A cache is an ordinary overlay, so no flag names caches. Each old flag fails with exit status 2 and
names its replacement:

| Flag | Use instead |
| --- | --- |
| `save --caches [--cache <path>]` (removed, exit 2) | `save --overlays [--folder <path>]`: the parts of the folder overlays, with no named change; a named save (`save -m`) saves them too |
| `push`, `fetch`, `pull`, `sync`, `clone` `--caches` (removed, exit 2) | `push`, `sync` and `clone`: nothing, every overlay comes unless `--overlay` names fewer; `fetch` and `pull`: `--overlay base,<cache>` (a plain fetch takes only the caches of `main`, the protected lines and the save-back lines) |
| `fetch --no-caches` (removed, exit 2) | `fetch <remote> --overlay base` |
| `fetch`, `clone` `--caches-of <line>` (removed, exit 2) | `fetch <remote> --line <line>`, `clone <url> --line <line>` |
| `merge <line> --caches-only` (removed, exit 2) | `merge <line> --overlays-only` |
| `fsck --caches` (removed, exit 2) | `fsck --overlays` |

`layr fetch <store remote> --line <name>[,<name>] --overlay <layer>[,<layer>]` and the same flags on `push`: `--line` takes the history of those lines (their heads with `--heads`) and, in each overlay, the history of their overlay lines; `--overlay` takes only those layers, and `base` names the base. A state left out stays on the remote and comes on first read. Example for a merge runner on a Mac: `layr fetch cloud --line main --overlay base,target.macos-aarch64`. A fetch or push leaves out an overlay whose `platform` is another platform's, unless `--overlay` names it. A plain fetch (no `--line`, no `--overlay`) takes the records of a cache overlay and only the caches of `main`, the protected lines and the save-back lines; the other cache states stay on the remote. `fetch <remote> --line <name>` or `--overlay <cache>` takes them, and a plain `clone` takes the usable caches of this platform ([remotes](remotes.md)). `remote add` of a server that cannot be reached adds the remote with a warning; the first fetch or push checks it.
Use `--all-lines` to save auto states for all lines. `--all` applies only to named saves.
`--note` cannot be combined with `-m`, `-F`, or `--amend`.

`park` keeps unfinished work. It saves the changes of the working folder as the head of a new line
`parked.<line>.<n>` and resets the folder. The parked line has no working folder; it is an ordinary
line otherwise (`line list`, `show parked.main.1:<path>`, sync and push take it). Its head has the
line head as parent.
- `park list` lists the parked lines of the current line on this machine, newest first: `park@{0}: <line>: <message>`.
- `park show [<entry>]` shows the change. `park pop [<entry>]` merges it into the working folder and
  deletes the parked line; `park apply [<entry>]` keeps it. A conflict keeps the entry.
- `park drop [<entry>]` deletes one parked line, `park clear` all of them. `layr undo` brings one back, without a folder.
- An entry is `park@{n}`, a parked line name, or its head state. `apply` also takes any state with a parent.
- New files come back untracked.

Park entries of older layr versions were states on the line. The first `park` command after the upgrade
makes each one a parked line, in the same order. Until then they keep their trees.
`pick` applies a state. `replay` applies states on a new base. `invert` reverses a state.
Use `restore --ours` or `restore --theirs` for conflicts.
`merge`, `pick`, `invert` and `replay` never stop on a conflict: the state keeps the conflicts, and `--continue` says to fix the files and run `save -m`. `--abort` undoes the operation.

Git command names (`branch`, `commit`, `stash`, `checkout`, `switch`, `cherry-pick`, `rebase`,
`revert`, `worktree`) are no layr commands. They fail like any unknown command, with no hint.
The same holds for removed layr commands (`cache`, `layer`, `approvals`, `machines`, `git-sync`)
and removed options (`--no-commit`, `--autostash`, `--layers`).
Git commands under `layr git` keep Git words.

`usage [--days <n>] [--json]` shows how often each command ran on this machine in the last `n`
days (default 28): runs, runs by agents (with `LAYR_SESSION`), failed runs and users, per command
and subcommand. Every command adds to this count in `<store>/machine/usage.sqlite`. Only command
and subcommand words are kept, never arguments, paths or messages. The count stays on this machine.
It helps to decide which commands are used before one is removed.

`du [<line>...] [--json]` shows the disk space of each line on this machine: `total` is the
allocated size of its files, `unique` the part that no other line, state or clone shares, which
deleting the line frees. A file with several links counts once. It reads only metadata and takes
no store lock. Each backend measures `unique` in its own way: APFS reports the private size of a
file; on btrfs, XFS and ext4 the extents that a snapshot or reflink shares carry a shared flag
(`FIEMAP`); an OverlayFS line owns only its upper layer.

`du --store` covers the whole store: each line folder, each other folder of a project (`states`,
`deltas`, `git`, `meta`) and `trash`. For each part it shows `frees`: the bytes that only this part
holds, also data that its own files share among themselves, which deleting the part frees. Shared
data counts once under a key that names it, and belongs to a part when no other part references it.
The last lines show the data that several parts share and the store total. On btrfs, the bulk scan
counts whole allocated extents, including compressed and partially referenced extents. It reports
committed on-disk allocation, so writes from the current transaction may not appear yet. On
OverlayFS, data awaiting writeback can count more than once when seen through a view and its layer.

Storage is freed when a state loses its last owner (docs/ownership.md). A command that drops
the last owner of a state moves its storage to the trash after its commit and after the store
lock, so `line delete`, a save past the rings, and a moved head free space in that command. A
crash between commit and move leaves the state queued; the next command finishes it.

`gc` checks the owners. It compares the owners table with a replay of the records and reports
the rows that differ and the present states that no owner names (leaks). It exits 1 when it
finds any. It prints one line with the number of leaked states and their bytes, which
`gc --repair` frees. `gc --repair` rebuilds the table from the records, frees the leaks, releases
expired refs and dead pins, removes the old cache snapshots of states, and empties the
trash. `gc -n --repair` prints what it would free. `fsck` runs the same check, and
`fsck --repair` rebuilds the table. `gc --dedup` shares identical cache files and is unchanged.

`gc --repair` also frees the trees of imported states that no line head, park entry, overlay, undo
record or auto state needs, and that git builds again. The record stays; the next read builds the
tree (`docs/ownership.md`, "States imported from Git"). `gc -n --repair` lists them. A plain `gc`
only checks.

`gc` also compares the `bases` table (the base of each thin layer) with the layers on disk, and prints
each difference. `gc --repair` makes the table match the disk.

### Schema upgrades

For a schema upgrade, stop the service/daemon first, upgrade every CLI and the daemon binary,
then restart the service. The first project open rebuilds its database before running the command.
The CLI and daemon must support the same database schema even when their local store format is
unchanged. Their negotiation checks this before a command is sent; upgrade both if it refuses.

A layr upgrade that changes the schema rebuilds the derived tables from the records in one
`BEGIN IMMEDIATE` transaction. The backfill, table/index changes and schema version commit
together. If the process crashes before that commit, SQLite recovery discards the unfinished
transaction: the old schema and records remain, and the next open retries the upgrade.
Concurrent readers continue to see their consistent pre-upgrade snapshot; new reads see the
rebuilt tables after commit.

Layr keeps no pre-upgrade database copy. An older layr cannot open a database after the upgrade;
there is no automatic downgrade. To retain a downgrade option, make your own consistent store
backup while all store users and the daemon are stopped, before upgrading, and restore that
backup before using the old binary again. The rebuild also removes the copies
`layr.sqlite.schema-<old>.bak` that older layr versions made.

Keys that keep states (count rings, `layr config <key> <n>`):

| key | default | keeps |
|---|---|---|
| `retention.auto` | 10 | the newest automatic states of each line |
| `undo.ops` | 20 | the states named by the newest operations of each line (`layr undo` reverses them) |
| `undo.deleted` | 5 | the final and staged states of the newest deleted lines (`layr undo` brings a line back) |

An
operation past `undo.ops` cannot be undone: `layr undo` says that the states of the operation
were freed. A state that a store remote holds (a successful `layr push <store-remote>` names it, the remote
still has the recorded URL, and this machine reads that store) is kept while eviction is off (the
default). With `gc.evict on`, `gc --repair` evicts such a state: its tree
goes when no local owner of the strong kinds (head, auto, undo, deleted, park, overlay, pin) names
it, no cache or cache overlay needs it, and the store format can restore its tree exactly. Its record stays, and a read fetches the tree back.

A state that a live thin layer (APFS) uses as its base keeps its storage until that layer goes.
`gc --all-projects` repairs the owners table and finishes the free queue for every project, but frees no
leaked state: a state that a command has recorded and not yet given an owner looks like a leak for a
moment. Only `gc --repair` frees leaks (the Linux daemon runs the same repair without freeing leaks; the
macOS user daemon runs no gc).

Test only (tests/t99-ownership-free.sh): `LAYR_TEST_EXIT_AFTER_COMMIT=1` exits with status 86
after a commit that queued states, before the free step. Test only (tests/t100-pin-reads.sh):
`LAYR_TEST_PIN_HOLD_MS=<ms>` sleeps once per process after its pins are taken. Neither is for use
otherwise.

Reads of imported states (`log -p`, `log <path>`, `show`, `diff`, `blame`, `grep <rev>`) take the
bytes from Git and build no tree. `line new`, `try`, `bisect run` and `reset --hard` build the tree
they need (`docs/ownership.md`, "States imported from Git").
For imported history, `log -- <path>` and `show <rev> -- <path>` select changed paths from
Git trees before loading file contents. Missing or corrupt blobs outside the selected paths do
not fail these commands; selected blobs are still checked when their contents are loaded.
Without a path filter, content reads still load all changed files.

Revisions: `head`, `@`, `head~2`, `head^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids (full or a prefix of at least 4 digits), `park@{n}` (the head of
the n-th newest parked line of the current line), `head@{n}` (auto states, newest first). Every command that takes a revision takes a git commit id.

Two kinds of id: a state id has 32 hex digits, a git commit id has 40. `rev-parse` and `log` print
state ids. `layr rev-parse --git <rev>` prints the git commit of the state (in a line with a `.git`
folder, every head has one). `layr log` shows the git commit of an imported or pushed state on a
`Git:` line.
`HEAD` and `stash@{n}` fail. Use `head` and `park@{n}`.
The names `head` and `HEAD` are reserved for lines, tags, and refs.

Content ids: a state ID is the same only in copies that share its record, but the git ids of its
content are the same on every machine and in git. `layr rev-parse <rev>^{tree}` (and `%T`/`%t` in
`--format`) prints the tree of the commit that `layr git push` exports for the state. `layr rev-parse
<rev>:<path>` prints the id of one path in that commit, as `git rev-parse` does: a tree for a
folder, a blob for a file, the root tree for `<rev>:`. `./` and `../` resolve against the current
folder. A path that the exported commit does not have fails with exit 128.

## Remove a store

Layr seals the files of a store: on macOS each state has the immutable flag (`uchg`) and a deny entry in its ACL,
and on btrfs each state is a read-only subvolume. Thus `rm -rf <store>` fails. Use this command:

```sh
layr store remove /tmp/test-store
```

The command does these steps:

1. It refuses a folder that is not a layr store. A store has `store-version.json` or `machine/id`. On macOS it also
   takes what `rm -rf` left of a store: only sealed entries below `projects/`, and the folders that hold them.
2. It refuses the store of this shell (`LAYR_ROOT`, or the default store). Add `--force` to remove it.
3. It refuses a store that a layr command or daemon has open, or whose trash a layr process empties. Stop that
   process, then run the command again.
4. It unmounts each file system that is mounted below the store (for example a virtual line). The store folder
   itself must not be a mount point.
5. It opens each sealed entry and deletes the folder.

Volumes and moved line folders of the store are in other folders. The command keeps them and names each one.
The command does not open the store, so it works with a store of another layr version too.

## Workflow

1. A writer makes a line (`layr line new topic-1 main`), or a maintainer makes one for a user
   (`--owner bob`). Only the owner changes a line.
2. The user works and saves named states in that folder. A program that drives the user can call
   `layr save` at quiet points, and `layr overlay repair <overlay>` when a build tool made a cache
   folder again.
3. Review takes an exact state: `layr diff main...topic-1`, `layr try <state> -- <tests>` runs the
   command in a temporary copy of that state and records the result.
4. A maintainer runs `layr merge topic-1 --expect <state>` in the protected main line: it merges
   exactly the reviewed state and refuses if the line moved. When main's rule names checks
   (`layr config check.unit 'PATH=$HOME/.cargo/bin:$PATH cargo test'; layr protect main --check
   unit`), they run on the merge result as the caller, in a clean environment (the command sets
   what it needs), without the project lock, and main moves only if they pass and
   main did not move in the meantime. A merge never stops on a conflict: the new state lists the
   conflicts, text files get markers, and `layr status` shows them. A line with checks takes no
   result with conflicts: they are resolved in a line first.
5. `layr git push origin <branch>` exports the line's history: each state the remote lacks becomes
   a git commit with the same parents. `--squash [-m <message>]` exports one commit on top of the
   last exported state. The exact commit id is recorded before the push; after a lost response,
   a repeated push reads the remote ref first. With `push.require_check true`, a push needs a
   passing `layr try` of that exact state. A push refuses when the line started on states of
   another line that no push carried yet (for example local work on main): the refusal lists
   them with the line that made each one. Start the line from the remote branch and pick your
   states. To publish that work on purpose, name its lines: `--include-base main[,<line>]`. A
   merge of another line into this line is not refused.

The design and the reasons for it, including what layr leaves to programs built on it, are in
`docs/design.md`.

## Checkpoints for tools

A program that drives agents (an editor, an agent app) keeps a state of the folder per step
without moving the line head:

```sh
layr save --note "turn 12" --keep app/thread-7/12   # an automatic state, kept under that name
layr keep list --json                               # [{"name": "app/thread-7/12", "state": "<id>"}]
layr diff --numstat -z --exclude-standard <a> <b>   # the step's changes, ignored files left out
layr restore --source <a> --clean -- .              # the folder as it was, in one operation
layr keep drop app/thread-7/12                      # release it (the line delete releases all)
```

- A keep holds its state on this machine only (docs/ownership.md, `keep`): no record makes it, so it
  never reaches a remote, `layr undo` never reverses it, and the `retention.auto` ring never drops
  it. An unchanged folder makes no new state; the keep then names the state equal to the folder.
- An automatic state holds the whole folder, ignored files too. In a diff of two states,
  `diff --exclude-standard` leaves out a file that the ignore rules of its side ignore (the new
  side, or the old one for a deletion) and that side's tracked tree lacks: an automatic state's
  line head, or a named state itself. A tracked file that an ignore rule matches stays, as in git.
- `restore --clean` removes the new files that the source does not have, as `clean -fd` (ignored
  files stay), in the same operation as the restore. The index is not changed; `layr reset -q -- .`
  puts it back to the head.
- Run these with your own `LAYR_SESSION`, so the user's `layr undo` takes the user's operations only.

## Reading and testing other states

A user with the reader role (or a line of their own) reads every state of the project through
the service. A line is not needed: name the project with `--project` (and a line with `--line`
when the command reads a line's head, index or working content). Inside a line folder, `--line`
alone names another line of the same project.

| Task | Command |
| --- | --- |
| history of a line | `layr --project app --line main log --oneline` |
| one file of a state | `layr --project app show <state>:<path>` (`<state>:<folder>` lists the folder) |
| changes between two states | `layr --project app diff <state1> <state2>`, `diff main...topic-1` |
| files of a line's index | `layr --project app --line main ls-files` |
| search a state | `layr --project app grep <pattern> <state>` |
| git ids of a state | `layr --project app rev-parse <state>^{tree}`, `rev-parse <state>:<path>` |

`layr try <state> -- <command>...` runs a command in a temporary copy of any state the caller may
read, for example tests of another user's line:

```sh
layr --project app try topic-1 -- cargo test          # the head of bob's line topic-1
layr try head~1 -- sh -c 'git log -1 && make check'  # an older state of your own line
```

`layr try <state> --check <name>` runs the command of `layr config check.<name>` exactly as protected
refs, merges and the queue run it (`sh -c '<command>'`). Its passing result is the check those rules
want.

- The copy is a new folder `lines/.try-<random>` that belongs to the caller. It holds the full
  content of the state, a `.git` folder whose `HEAD` is the state's commit (see "Git in a line"),
  and warm caches by the usual rules. `git status` in it is clean.
- The command runs as the caller, from the root of the copy, with the caller's environment
  except the variables that would point git or layr elsewhere (`GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_COMMON_DIR`, `LAYR_LINE`). `LAYR_TRY_STATE` is
  the state id and `LAYR_PROJECT` the project.
- The caller's line does not change. The copy is deleted afterwards (`--keep` keeps it and prints
  its path); the exit code is the command's, and the result is recorded as a check of the state
  (`layr attestations <state> --kind check`).
- It needs the `try` action (the reader role, or a line of your own), not write access.

## Git in a line

In a project with a git store (a clone, or a project with a remote), each line has a `.git`
folder for tools that call git. Its objects come from the project store. After every move of the
line's head (save, amend, merge, reset, undo, pull, push), `HEAD` is the commit of the new head:
the commit that `layr git push` sends for that state, with the commit of the previous state as its
parent, and the index of that state. So `git rev-parse HEAD` changes only when the head moves,
`git status` shows only the uncommitted changes, and `layr rev-parse <commit>` finds the state of
such a commit. The commit is made in the local git store only; nothing leaves the machine before
a push. A push with `--squash` sends another commit for the same state; `HEAD` then shows that
commit. `layr git sync` also records the commits of the head and its ancestors, as a push does.
Git is read-only in such a line: a git commit, merge or reset there (`HEAD` no longer shows the
commit of the head) makes the next save, merge, undo, reset, pull, push or `layr git sync` refuse,
and nothing moves. The message names the commit to reset `HEAD` to (`git reset --hard <commit>`
undoes the git change); then the command can run again.
A `.git` folder that the line owner made instead is left alone.

`layr log --json` writes one state JSON object per line. History filters and `--auto` apply before output.

The `--json` output of `status`, `save`, `line list`, `queue list`, `change list`, `change show` and `attestations`
has a JSON Schema. `layr schema <name>` prints the schema of the installed version. See [JSON output](json.md).

`layr line list --json` writes an array of local lines. Each line includes its ID, name, head, owner, machine, and local path.
A name pattern filters the array. JSON output does not support `--all` or `--remotes`.

`layr change list --json` writes an array of changes. Each change has `change_id`, `number` (null until the
numbering machine gives one), `title`, `description`, `status` (`open`, `closed` or `merged`), `closed` (only for a
closed change), `versions`, `state` (the newest version) and `message`. `change show --json` has the same `number`,
`title`, `description`, `status` and `closed` fields. See [review](review.md#change-numbers-titles-and-closing).

`layr change diff <id> --from vN --to vM --json` writes the change ID, version labels, and interdiff text.
`--since-review` starts at the newest version that you approved (an approval of this machine and your user), so
a reviewer sees only what changed after their review. With `--person <login>` it starts at the newest version that
person approved through a signed request (`layr request approve --person`).
The `files` array gives each changed file patch, its status, and added and removed line counts.
Each version is compared with its own first parent. Changes to the base stay out of the interdiff.

The overlay commands below work on the model of [overlays](overlays.md) section 1: lines and overlays are independent, a line is the same line (same id and name) on every overlay, and the view of a line folder joins the line with a set of overlays. `layr overlay migrate-parts` moves a store of layr before 0.44 (old stacks, `line-<line id>` names, the `cache` flag).

Hidden overlays use follow mode. Only their readers see them in lists and reports.
Use `layr overlay show-to-project <name>` to make one visible to project readers.
This change is recorded and cannot be reversed.

`layr overlay show <name> --json` writes an object with `name`, `hidden`, `access_mode`, `readers`,
`managers`, `epochs`, `patterns`, `peers`, `state`, and `properties`. Each reader has a canonical
`principal` and a `kind` (`user`, `group`, `everyone`, `machine`, or `person`). Person
principals contain the login without the age recipient. Managers are machine UUIDs.
Epochs have `number`, `id`, `time` (milliseconds), and `machine`; `reason` appears only
when the signed key record contains it. Numbers start at 1 in signed key record application
order (time, machine, sequence), including repeated records for the same epoch ID.
`patterns` defaults to an empty array; routing patterns are not built. `peers` is the
configured machine array, or null for the default peer policy. `state` is the head selected
by the ordinary overlay history command, or the overlay's empty root state when it has no line.

`layr overlay log <name> --ops [--json]` reads all signed operations from the overlay's
own log, newest first. JSON writes an array of `op`, `actor`, `time` (milliseconds), and
`id` (record UUID). Operation names use CLI words such as `state`, `park`, and `replay`.
Both commands require overlay read access. A hidden overlay is indistinguishable from an
absent overlay to a non-reader, including errors and exit status. Neither command prints
key material or operation payloads. Without these flags, the existing state history output applies.

## JSON for tools

The JSON of `save`, and the line names in the JSON of `change`, `queue` and `status`.

### save --json

`layr save -m <message> --json` (also with `-a`, `--amend`, `-F <file>`) writes one JSON object in place of
the text. The files that the save left out are still named on stderr.

```json
{"saved": true, "state": "<state uuid>", "short": "a1b2c3d", "line": {"id": "<line uuid>", "name": "w"},
 "message": "feat: one", "parents": ["<state uuid>"], "amend": false, "conflicts": 0,
 "files": {"changed": 1, "insertions": 1, "deletions": 0}}
```

- `state` is the state UUID, as in other JSON output. `short` is the short id of the text output.
- `files` gives the numbers of the text summary. `conflicts` counts the files that still have
  conflict markers.
- With nothing to save, the object is `{"saved": false, "state": null, "short": null, "line": {...},
  "reason": "nothing to save"}` and the exit status is 1, as in text.
- In a line with overlays, the object also has `overlays`: `[{"overlay": "<name>", "state": "<uuid>",
  "short": "<id>"}]`. When only overlays changed, `state` is null and `saved` is true.

`layr save --json` and `layr save --overlays --json` write one object per line (one line of output for
each line; `--all-lines` writes several):

```json
{"saved": true, "state": "<state uuid>", "short": "a1b2c3d", "line": {"id": "<line uuid>", "name": "w"},
 "overlays": [{"path": "target", "overlay": "<overlay>", "state": "<uuid>", "short": "<id>"}]}
```

`state` is the state equal to the working folder. `saved` is false when the folder did not change: then
`state` is the existing state. `overlays` lists the cache overlays that `--overlays` saved (empty
without it). Notes that the text prints, for example a skipped folder, go to stderr.

### Line names in JSON

Where JSON gives a line id, a sibling field gives the line name. The id field does not change.

- `change show --json`: `versions[].state.line_name` next to `line`, and
  `versions[].operation.target_line_name` next to `target_line`.
- `change list --json`: each row has `line` (the line id of the newest version, or null) and `line_name`.
- `queue add --json`, `queue list --json` and `queue run --json`: `target_line_name` next to
  `target_line`. `into` keeps the name that the entry got when it was added.
- `status --json`: `line` is the line name; `line_id` gives its id.

The name is the current name of the line. A deleted line keeps its last name. The name is null when
the project has no record of the line.
