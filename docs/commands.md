# Command reference

This page is part of the [Layr documentation](../README.md#documentation).

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status [--json]`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch [<prefix> <rev> --count N] [--json]`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `attest <kind> <rev> [--payload <file>]`, `attestations <rev> [--kind <kind>] [--json]` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] [--json] -- <cmd>`, `gc`, `fsck`, `reindex` |
| Git bridge    | `git remote`, `git fetch`, `git pull`, `git push [--squash]`, `git clone`, `git sync`                             |
| remotes       | `remote add\|list\|self\|remove\|trust`, `sync <remote>`, `push <remote>`, `fetch <remote>`, `pull <remote>`, `clone <url>` |
| OCI           | `push <oci-remote> <rev> [--runnable <name>] [--json]`, `access token <principal> [--until <duration>]`, `access token --revoke <principal>` |
| records       | `op export`, `op import` (JSONL) |
| copies        | `export <folder>`                                                                              |
| views         | `view [<rev or line>] [--json]`, `view save [<line>] [--json]`, `view show <id> [--json]` |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow]`, `overlay list`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>`, `overlay move [--purge] <name> <paths>`, `overlay log\|show\|diff\|grep\|ls-files <name>` |
| refs          | `ref set <name> <rev> [--expect <rev>] [--ttl <dur>]`, `ref list [<prefix>]`, `ref log <name>`, `ref delete <name> [--expect <rev>]` |
| queue         | `queue add <rev> [--into <line>]`, `queue list [--into <line>] [--json]`, `queue remove <entry>`, `queue run [--into <line>]` |
| probes        | `probe run <rev> [--probe <name>] [--json]`, `probe diff <base> <head> [--json]` |
| changes       | `change list`, `change show <id>`, `change log <id>`, `change diff <id> [--from vN] [--to vM]` |
| setup         | `init`, `projects`, `config`, `cache add|remove|list|repair`, `daemon`                        |

Revisions: `HEAD`, `@`, `HEAD~2`, `HEAD^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids of imported or pushed states, `stash@{n}`, `HEAD@{n}`
(automatic states, newest first).

Content ids: a state ID is the same only in copies that share its record, but the git ids of its
content are the same on every machine and in git. `layr rev-parse <rev>^{tree}` (and `%T`/`%t` in
`--format`) prints the tree of the commit that `layr git push` exports for the state. `layr rev-parse
<rev>:<path>` prints the id of one path in that commit, as `git rev-parse` does: a tree for a
folder, a blob for a file, the root tree for `<rev>:`. `./` and `../` resolve against the current
folder. A path that the exported commit does not have fails with exit 128.

## Workflow

1. A writer makes a line (`layr branch topic-1 main`), or a maintainer makes one for a user
   (`--owner bob`). Only the owner changes a line.
2. The user works and commits in that folder. A program that drives the user can call
   `layr save` at quiet points, and `layr cache repair` when a build tool made a cache folder
   again.
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
   passing `layr try` of that exact state.

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
layr try HEAD~1 -- sh -c 'git log -1 && make check'  # an older state of your own line
```

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
line's head (commit, amend, merge, reset, undo, pull, push), `HEAD` is the commit of the new head:
the commit that `layr git push` sends for that state, with the commit of the previous state as its
parent, and the index of that state. So `git rev-parse HEAD` changes only when the head moves,
`git status` shows only the uncommitted changes, and `layr rev-parse <commit>` finds the state of
such a commit. The commit is made in the local git store only; nothing leaves the machine before
a push. A push with `--squash` sends another commit for the same state; `HEAD` then shows that
commit. `layr git sync` also records the commits of the head and its ancestors, as a push does.
A `.git` folder that the line owner made instead is left alone.

