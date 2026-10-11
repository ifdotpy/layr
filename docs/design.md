# layr design

This file records the design decisions of layr and the reasons for them. The README describes
the commands; this file says why they are shaped the way they are.

## 1. What layr is

layr is version control for one Linux machine with many users and many working folders, on
btrfs. Git and its hosts (GitHub and others) stay the remote. layr is a general tool: it
knows Unix users and groups, projects, lines, states, caches, records, rules and checks. It
knows nothing about the programs that use it.

**The boundary test.** A feature belongs in layr when a team of people on a shared Linux server,
without any special tool on top, would want it with the same meaning. Otherwise it belongs in
the program on top. Examples of programs on top: an orchestrator of AI coding agents, a CI
system, an editor.

- layr gives mechanisms with neutral defaults. The program on top chooses the values: which
  role a user gets, which checks are required, when to save, who builds a warm cache.
- A program on top uses only the public interface: the commands with their machine-readable
  output, and the record log (read only). It never reads the database or the store folders.
- Every security rule is enforced by layr. A rule that only the program on top checks can be
  bypassed by calling layr directly; so the program on top expresses its limits as layr rules
  (roles, deny rules, protection rules).
- A program on top may label operations (`LAYR_SESSION`). layr stores, signs and shows labels
  and never decides permissions by them.

### Why not git

Git was made for one person with one working folder on a machine that this person trusts, and
a remote host as the place of record. Its rough edges follow from these assumptions. layr starts
from other ones: many writers on one machine, people and programs, and not every writer is
trusted with every line.

| Git assumes                                  | What breaks with many writers                                                                | layr                                                                                                      |
| -------------------------------------------- | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| The working folder is not history            | Uncommitted work is invisible to others and is lost with `reset --hard` or `rm -rf`          | The working content is history: automatic states before every changing command, on `save`, and every 10 minutes; `undo` and `op log` |
| One working folder per clone; a branch is a name | Worktrees, `stash` and `checkout` to move between branches                               | A line is a folder; a new line costs the same at any size                                                 |
| Whoever can write `.git` owns it             | Hooks and local rules are advice: `--no-verify`, `update-ref` or deleting a hook goes past them; protection lives only on the host | Users, groups, roles, deny rules and protected lines, enforced by the root service on the machine itself  |
| A merge takes what a branch name points to now | A change made after the review goes in unseen                                              | `merge --expect` takes exactly the reviewed state; checks run on the merge result before the line moves  |
| Build output and dependencies are not its concern | Every new working folder builds from nothing and keeps its own copy                     | Caches: regenerable folders outside history; a new line starts with a copy-on-write copy (section 5)      |
| History is an object store; files must be checked out | Reading an old state or starting a new folder costs a write of every file          | A state is a read-only folder; `show` and `grep` read it directly                                         |
| A conflict stops the operation               | Work waits until someone resolves it                                                         | A merge never stops: the conflicts are part of the new state (a line with checks still takes none)        |

The same ideas appear in other systems. Jujutsu makes the working copy a commit, keeps an
operation log with undo, and lets conflicts be committed. Sapling with EdenFS gives working
copies that do not write every file. layr reaches these ideas from the storage side: btrfs gives
snapshots in constant time and change lists in O(changes), so the working folder can be history
without a cost for each file. It adds two things that these systems leave to the host: rules
that the machine itself enforces, and regenerable caches.

The cost is the reason git stays at the edge. layr has its own store, so it runs only on Linux
with btrfs, while editors, hosts and CI systems read git. layr imports from git, pushes states as
git commits, and exports plain folders; it does not try to be git on disk.

## 2. Why a root service

`btrfs send` (the source of every change list in O(changes)) and snapshots into protected
folders need root. So one root service does every operation and every other user is a client
(`SO_PEERCRED` gives the caller's uid). This also makes layr the only gate: history, other
users' lines and the database are not readable by clients except through layr. The service
never uses root power for a caller's choice: commands named by a caller run as the caller,
paths named by a caller are opened with the caller's file permissions, and writes into lines
go through descriptors that cannot follow a link out of the line.

## 3. Who may do what

The model takes its parts from systems that have stood up in practice:

| Part | From | Rule in layr |
|---|---|---|
| default deny, deny always wins, order of rules does not matter | AWS Cedar, IAM | a deny rule takes an action away from anyone except root |
| roles as named sets of actions | GitLab, Gerrit | reader < writer < maintainer < admin |
| rules on line patterns, most restrictive wins | GitHub rulesets, Gerrit | protection rules of all matching patterns combine |
| exact revision before a change lands | Gerrit, "not rocket science" rule | a protected line takes only `merge --expect` of a reviewed state, and its check passes on the merge result before the line moves |
| owner of a working copy | Unix | only the owner writes in a line |

**Principals.** A Unix user, a Unix group, or everyone (`*`). Root is the machine admin: it may
do everything, and every record names who acted.

**Roles** (per project, per machine, because uids are local to a machine):

| Role | Actions |
|---|---|
| reader | read history (`log`, `show`, `diff`, `grep`, `blame`), `try`, `export` |
| writer | create own lines |
| maintainer | update protected lines (per their rules), tags, push, create lines for other users, delete or adopt other users' lines |
| admin | roles, deny rules, protection rules, checks, remotes, configuration, caches, replication and machine trust, backups, gc, record import and export, fsck |

A user's role is the highest role of the user, the user's groups and `*`. Owning a line in a
project includes reading the project.

**Lines.** A line belongs to one user. Only that user changes it (commit, reset, stash and the
other work commands). Nobody else writes into it, the admin included. To change another user's
line, make a line from it and merge, or delete it (maintainer). Root can do everything.

**Roles for a time.** `layr access grant <principal> <role> --until <30m|2h|1d>` adds a role
that ends by itself, for example a maintainer role for one task; the principal keeps its role
after that.

**Deny rules.** `layr access deny <principal> <action>...` takes actions away from a user, a
group or everyone, whatever the role. The actions are `read`, `try`, `export`, `line.create`,
`merge` (merges into protected lines), `line.manage`, `tag`, `push`, `access`, `config`,
`remote`, `sync`, `backup`, `gc`, `records` and `fsck`. This is how a program on top limits its users, for
example: the group of AI agents may not push, sync or trust machines.
`access` covers the roles, deny rules, protection rules and the commands of named checks
(`check.<name>`), so a user without it cannot weaken a protected line by any setting.

**Protection rules** for line name patterns (`main` is protected when a project is made):

| Setting | Default | Meaning |
|---|---|---|
| `update` | maintainer | lowest role that may merge into the line |
| `expect` | true | a merge must name the exact reviewed state (`--expect`) |
| `check` | none | named checks (`layr config check.<name> '<command>'`) that must pass on the merge result before the line moves; the command runs as the caller in a temporary copy of the result with a clean environment (it runs code that may come from another user): only HOME, USER, LOGNAME and `PATH=/usr/local/bin:/usr/bin:/bin`, so the command sets what else it needs, for example `PATH=$HOME/.cargo/bin:$PATH cargo test`. A passing check of the same state and command is reused |
| `direct` | false | the owner may also commit, reset or apply in the line directly |
| `rewrite` | admin | lowest role that may move the line to a state that is not a descendant (reset, undo of a merge) |
| `paths` | none | paths (a folder, a file or a glob) whose change needs a higher role than `update`, for example `.github=admin`: a merge that changes such a path needs that role (`layr protect main --path .github=admin`) |
| `approvals`, `approvers` | 0, maintainers | approvals the reviewed state needs (`layr approve <state>`, a signed record of exactly that state). An approval counts when its user is not an author of the states the merge brings in and matches every listed approver principal (with none listed: has the maintainer role). A program on top that wants "a person must approve" lists a group of people, for example `--approvals 1 --approvers @humans` (SLSA source level 4 asks for two persons: the author and an approver) |

When several patterns match, the most restrictive value of each setting applies.

The check runs without the project lock. After it, the merge goes on only if the line did not
move in the meantime; otherwise the caller merges again. A merge result with conflicts is not
taken by a line with checks: the conflicts are resolved in another line first. So a protected
line only takes states whose exact content passed its checks.

## 4. History on the remote

`layr push` exports every state of the line's history that the remote does not have as its own
git commit, with the same parents (merges stay merges). The same state always gives the same
commit. `layr push --squash` exports the pushed state as one commit on top of the last exported
one; a program on top that wants one commit per task uses it.

## 5. Warm caches

A cache is a folder of regenerable content, such as a Rust `target` folder, kept as a nested
subvolume, outside states, diffs and backups. Every working folder would otherwise build from
nothing, and the same build output would be on disk once per folder.

layr keeps caches as storage and knows nothing about builds. It cannot tell whether build output
is valid for a state, so it never decides that by itself:

- A new line (and the temporary copy of a check) takes for each cache the snapshot of its start
  state, or else of the nearest ancestor state on this machine that has one. The copy shares
  all blocks; only its files change owner. The build tool then decides what to rebuild.
- Which folders are caches: the paths of `layr cache add`, and the folders that `.layrattributes`
  files mark `regenerable`. A `.layrattributes` line is a pattern of the `.gitignore` form and
  attributes (`node_modules regenerable`; `-regenerable` unsets it; the last matching line wins);
  a file applies to its folder. It is part of the project, so on a protected line it changes
  only through review. A regenerable folder is a cache only if the ignore rules ignore it: a
  cache must not hide files that are in history. A new line takes a regenerable folder only
  warm, while its `.layrattributes` still marks it.
- Caches come only from `layr save --caches`, the explicit choice of someone who may save the
  line. It first turns the regenerable folders into caches, so their files stay out of the state. On a
  protected line with no changes since its head (ignored files do not count), it gives the caches
  to the head, so the next lines from that head start warm; else to a new automatic state.
- A cache made by another user goes to a line only if it was saved on a protected line, because
  a cache holds code that review does not see. Other caches go only to lines of the user who
  made them.
- A check starts from the warm caches of its state, but its build output is not kept.
- gc keeps the cache snapshots of line heads and of the newest `retention.caches` (5) states
  with caches; the states themselves stay.

A cache helps only a tool that checks its own output and accepts a new path, because every line
has its own folder. Cargo's `target` does: Cargo compares fingerprints and rebuilds what a change
or the new path made stale. A Python virtual environment does not: its scripts keep the absolute
path of the folder where it was made, so in a copy they run the interpreter and packages of the
original line. pnpm links packages by relative paths, but the scripts in `node_modules/.bin` set
`NODE_PATH` to absolute paths of the original folder. Xcode and SwiftPM do not reuse build
output at another path. Which folders are caches, who builds them, and when a build is good
enough to save, is the choice of the program on top.

## 6. What is not in layr

- Choosing between several attempts at one task, and marking the others stale: a workflow of
  the program on top. It uses lines, `merge --expect` and deny rules.
- Copies of a line on another file system that are collected back (a host without btrfs): the
  program on top builds this from `export`, `diff` and the line folder itself.
- Any notion of agents, tasks, turns or a lead.

## 7. Planned

- Overlays: file trees with their own owner, readers and history, stacked on a line (`docs/overlays.md`).
- A dry-run mode for rule changes that reports what would be refused (GitHub rule evaluation).

## 8. Decided against

- **Keeping the build output of checks as caches.** A passing check of a protected line used to
  give its build output to the state as a shared cache. Removed on 2026-10-10: a check passing
  does not show that its build output is valid at another path or with another toolchain (a
  copied Python environment runs the original line's interpreter), and the output holds code
  that no one reviewed. A state is a key of sources only, not of the compiler and environment
  that a build cache needs. Caches now come only from an explicit `save --caches`.
- **Working folders outside the store.** A line's folder stays at
  `projects/<p>/lines/<name>`: snapshots, deletes and writes of the service go through the
  store's own folders, which no user can replace. A link (`ln -s <line folder> ~/code/topic`)
  puts a line at any path; layr finds the line from the real path.
- **Handing rights on by any user** (delegation chains as in Biscuit or UCAN). A user cannot
  share writing in its line through layr, because the line folder is a Unix folder of that user;
  and chains of rights from users who are not admins make the effective rights hard to see.
  Instead the admin gives a role for a time (`layr access grant <principal> <role> --until 2h`),
  which adds to the principal's role and ends by itself.
