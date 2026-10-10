# layr design

This file records the design decisions of layr and the reasons for them. The README describes
the commands; this file says why they are shaped the way they are.

## 1. What layr is

layr is version control for one Linux machine with many users and many working folders, on
btrfs. Git and its hosts (GitHub and others) stay the remote layer. layr is a general tool: it
knows Unix users and groups, projects, lines, states, layers, records, rules and checks. It
knows nothing about the programs that use it.

**The boundary test.** A feature belongs in layr when a team of people on a shared Linux server,
without any special tool on top, would want it with the same meaning. Otherwise it belongs in
the program on top. Examples of programs on top: an orchestrator of AI coding agents, a CI
system, an editor.

- layr gives mechanisms with neutral defaults. The program on top chooses the values: which
  role a user gets, which checks are required, when to save, who builds a warm layer.
- A program on top uses only the public interface: the commands with their machine-readable
  output, and the record log (read only). It never reads the database or the store folders.
- Every security rule is enforced by layr. A rule that only the program on top checks can be
  bypassed by calling layr directly; so the program on top expresses its limits as layr rules
  (roles, deny rules, protection rules).
- A program on top may label operations (`LAYR_SESSION`). layr stores, signs and shows labels
  and never decides permissions by them.

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
| admin | roles, deny rules, protection rules, checks, remotes, configuration, layers, replication and machine trust, backups, gc, record import and export, fsck |

A user's role is the highest role of the user, the user's groups and `*`. Owning a line in a
project includes reading the project.

**Lines.** A line belongs to one user. Only that user changes it (commit, reset, stash and the
other work commands). Nobody else writes into it, the admin included. To change another user's
line, make a line from it and merge, or delete it (maintainer). Root can do everything.

**Deny rules.** `layr access deny <principal> <action>...` takes actions away from a user, a
group or everyone, whatever the role. The actions are `read`, `try`, `export`, `line.create`,
`merge` (merges into protected lines), `line.manage`, `tag`, `push`, `access`, `config`,
`remote`, `sync`, `backup`, `gc`, `records` and `fsck`. This is how a program on top limits its users, for
example: the group of AI agents may not push, sync or trust machines.

**Protection rules** for line name patterns (`main` is protected when a project is made):

| Setting | Default | Meaning |
|---|---|---|
| `update` | maintainer | lowest role that may merge into the line |
| `expect` | true | a merge must name the exact reviewed state (`--expect`) |
| `check` | none | named checks (`layr config check.<name> '<command>'`) that must pass on the merge result before the line moves; the command runs as the caller in a temporary copy of the result with a clean environment (it runs code that may come from another user), and a passing check of the same state and command is reused |
| `direct` | false | the owner may also commit, reset or apply in the line directly |
| `rewrite` | admin | lowest role that may move the line to a state that is not a descendant (reset, undo of a merge) |

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

## 5. Warm layers

A layer is a folder of regenerable content (`target`, `node_modules`, `.venv`) kept as a nested
subvolume, outside states, diffs and backups. Every working folder would otherwise build from
nothing, and the same build output would be on disk once per folder.

- A new line (and the temporary copy of a check) takes for each layer the snapshot of its start
  state, or else of the nearest ancestor state on this machine that has one. The copy shares
  all blocks; only its files change owner. A build tool then rebuilds what changed.
- A layer made by another user goes to a line only if it was made on a protected line, because
  a layer holds code that review does not see. Other layers go only to lines of the user who
  made them.
- The checks of a protected line build the state the line takes. After a passing check its
  layers become the layers of that state, shared. So the protected line gets warm build output
  without a separate build, and the next checks start warm too. `layr save --layers` keeps the
  layers of a line's working folder as well (shared when the line is protected).
- gc keeps the layer snapshots of line heads and of the newest `retention.layers` (5) states
  with layers; the states themselves stay.

Who builds, and when, is the choice of the program on top: it can run its checks on the
protected line, or build in its own line and `save --layers`.

## 6. What is not in layr

- Choosing between several attempts at one task, and marking the others stale: a workflow of
  the program on top. It uses lines, `merge --expect` and deny rules.
- Copies of a line on another file system that are collected back (a host without btrfs): the
  program on top builds this from `export`, `diff` and the line folder itself.
- Any notion of agents, tasks, turns or a lead.

## 7. Planned

- Lines outside the store: a working folder anywhere on the same btrfs file system, as git
  worktrees can be anywhere.
- Grants with an end time: a user hands a narrower set of its own rights to another user until
  a given time, and revoking a grant revokes what was made from it (Biscuit, UCAN).
- Approvals: signed records that name an exact state; a protection rule may require approvals
  from members of a group, and from a person other than the author (SLSA source level 4).
- Path rules: protection settings for paths, such as CI configuration (Perforce, CODEOWNERS
  with "all of" semantics).
- A dry-run mode for rule changes that reports what would be refused (GitHub rule evaluation).
