# Command reference

This page is part of the [Layr documentation](../README.md#documentation).

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status [--json]`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `save -m <message> [-a] [--amend]`, `restore`, `reset`, `park`, `clean` |
| history       | `log [--json]`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `line new <name> [<rev>] [--count N] [--json]`, `line list [--json]`, `line delete`, `line rename`, `tag`, `merge [--expect]`, `pick`, `invert`, `replay`, `adopt` |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `attest <kind> <rev> [--payload <file>]`, `attestations <rev> [--kind <kind>] [--json]` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `try <state> --check <name>`, `bisect list [--json]`, `bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] [--json] -- <cmd>`, `gc`, `fsck`, `reindex` |
| Git bridge    | `git remote`, `git fetch`, `git pull`, `git push [--squash] [--include-base <lines>]`, `git clone`, `git sync`                             |
| remotes       | `remote add\|list\|self\|remove\|trust`, `remote public <remote> on\|off`, `sync <remote>`, `push <remote>`, `push [--dry-run]` (every layer to each of its remotes), `overlay remote add\|list\|remove`, `fetch <remote>`, `pull <remote>`, `clone <url>` |
| OCI           | `push <oci-remote> <rev> [--runnable <name>] [--json]`, `access token <principal> [--until <duration>]`, `access token --revoke <principal>` |
| records       | `op export`, `op import` (JSONL) |
| copies        | `export <folder>`                                                                              |
| views         | `view [<rev or line>] [--json]`, `view save [<line>] [--json]`, `view show <id> [--json]` |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow] [--hidden] [--history all\|last\|<N>] [--quiet] [--merge three-way\|union-theirs] [--gc none\|lru:<size>]`, `overlay set <name> <properties>`, `overlay list [--json]`, `overlay show-to-project <name>`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>`, `overlay move [--purge] <name> <paths>`, `overlay repair <overlay>`, `overlay log\|show\|diff\|grep\|ls-files <name>` |
| refs          | `ref set <name> <rev> [--expect <rev>] [--ttl <dur>]`, `ref list [<prefix>]`, `ref log <name>`, `ref delete <name> [--expect <rev>]` |
| queue         | `queue add <rev> [--into <line>]`, `queue list [--into <line>] [--json]`, `queue remove <entry>`, `queue run [--into <line>]` |
| probes        | `probe run <rev> [--probe <name>] [--json]`, `probe diff <base> <head> [--json]` |
| changes       | `comment <change> <text> [--version vN] [--path <path>] [--line N] [--reply-to <id>] [--json]`, `change list`, `change show <id>`, `change log <id>`, `change diff <id> [--from vN] [--to vM]` |
| requests      | `request <kind> ... --person [--remote <name>] [--expires <epoch-ms>]`, `requests apply [--remote <name>] [--json]` |
| person        | `person keys [--paper-key-out <path>] [--site <url>] [--store <url>]`, `person show [--json]`, `person accept`, `access person-key <login> <public-key>` |
| setup         | `init`, `projects`, `config`, `daemon`                                                                              |

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

`line new <name> [<rev>] [--count N]` creates lines. Use `line list`, `line delete <name>`, or `line rename [<old>] <new>` to manage them.
A line is a folder. Use `cd` to enter it.
`line new` refuses an automatic state that holds unsaved work (paths that are not ignored and
differ from both its parent and the current head of its line): the new line would carry that work
into its saves and pushes. An automatic state with only
ignored build output is a warm start.

`reset <rev>` without `--hard` moves the head and keeps the files (mixed, as in git). It lists the
files that now differ from the new head and names `layr reset --hard <rev>`, which changes them.

`save -m <message>` makes a named state. `--message` is the long form of `-m`.
`save -F <file>` reads the message from a file. `--file` is the long form of `-F`.
`save --amend` replaces the current head state. Without a new message, it keeps the current message.
The signed operation records its actor, version kind, source state, target line, and target head.
`replay` and `pick` record the same version fields. Old states need no migration.

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
Use `--running`, `--force`, `--caches`, or `-q` with an auto save.
`--caches --cache <path>` saves only that cache path. A big cache such as build output takes a minute to save; a small one takes a second. It fails when the path is no cache path of the line.
Use `--all-lines` to save auto states for all lines. `--all` applies only to named saves.
`--note` cannot be combined with `-m`, `-F`, or `--amend`.

`park` keeps unfinished work. It has `list`, `show`, `pop`, `apply`, `drop`, and `clear` subcommands.
`pick` applies a state. `replay` applies states on a new base. `invert` reverses a state.
Use `restore --ours` or `restore --theirs` for conflicts.
Use `--no-save` with `merge`, `pick`, or `invert`. Use `--autopark` with `replay`.

Removed command names fail and show their replacements. They are not aliases.
`branch`, `commit`, `stash`, `cherry-pick`, `rebase`, and `revert` use the new names above.
`checkout` and `switch` are removed because a line is a folder.
The old `--no-commit`, `--autostash`, and `--layers` options fail and name `--no-save`, `--autopark`, and `--caches`.
The `cache` command group is removed. Its forms fail and name their replacements: `cache list` is
`overlay list`, `cache repair` is `overlay repair <overlay>`, `cache add` and `cache remove` are the
`regenerable` attribute in `.layrattributes`, and `cache pull` is `line new <name> <line>` or `merge <line>`.
Git commands under `layr git` keep Git words.

Revisions: `head`, `@`, `head~2`, `head^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids (full or a prefix of at least 4 digits), `park@{n}`, `head@{n}`
(auto states, newest first). Every command that takes a revision takes a git commit id.

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

## Reading and testing other states

A user with the reader role (or a line of their own) reads every state of the project through
the service. A line is not needed: name the project with `--project` (and a line with `--line`
when the command reads a line's head, index or working content).

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

`layr line list --json` writes an array of local lines. Each line includes its ID, name, head, owner, machine, and local path.
A name pattern filters the array. JSON output does not support `--all` or `--remotes`.

`layr change diff <id> --from vN --to vM --json` writes the change ID, version labels, and interdiff text.
The `files` array gives each changed file patch, its status, and added and removed line counts.
Each version is compared with its own first parent. Changes to the base stay out of the interdiff.

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
