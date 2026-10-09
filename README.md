# layr

Local version control for agent workspaces on btrfs. Git and GitHub stay the remote layer
(push, pull requests, CI). Everything local is btrfs: each line (branch and worktree in one) is a
writable snapshot, each state (commit) is a read-only snapshot, change lists come from
`btrfs send --no-data` in O(changes), and a merge copies files with reflinks.

The commands follow the git verbs and print git formats, so models that know git use layr
without training. Output of `diff`, `status --porcelain`, `show`, `log --format` and `--stat`
matches git byte for byte for the same content (tests/t08-gitcompat.sh).

Status: working implementation, Linux only (btrfs, kernel 5.6 or newer). Unsafe code is limited
to `src/sys.rs` (btrfs ioctls, fork, user switching, the user database); everything else uses
`rustix` and the standard library, and the crate denies `unsafe_code` outside that module. Built for the Codex
Studio Linux VM, where all agents run. The design contract is
`docs/btrfs-local-vcs.md` in the Codex Studio repository.

## Requirements

- Linux with btrfs for the store (a loop file is fine), `btrfs-progs`, `git`, `zstd`.
- Root for the service: snapshots into protected folders and `btrfs send` need it.

## Build and test

```sh
cargo build --release
sudo tests/run.sh            # integration tests: each file gets a fresh btrfs on a loop file
cargo test                   # unit tests
```

## Quick start

```sh
sudo layr daemon &                                   # the root service, /run/layr/layr.sock
sudo layr init app --from ~/src/app --owner lead     # or: layr clone <git url> app
cd /studio/projects/app/lines/main                   # a line is a folder
layr status; layr diff; layr add -A; layr commit -m "change"
layr branch worker-1 --owner agent1                  # a new line: O(1), its own folder
layr merge worker-1 --expect <state id the lead reviewed>
layr push origin task-123                            # one git commit for the task
```

The store lives under `$LAYR_ROOT` (default `/studio`). Users other than root talk to the
service; root can run every command directly.

## Concepts

| Term      | Meaning                                                                                     |
| --------- | ------------------------------------------------------------------------------------------- |
| state     | read-only snapshot + signed record (id, parents, author, message, conflicts). A commit.    |
| line      | writable snapshot at `projects/<p>/lines/<name>` with an owner and a head state             |
| auto state| a snapshot of the working content: before every changing command, at the end of an agent turn (`layr save --turn-end`), and every 10 minutes when the content changed (the service) |
| record    | one signed operation (Ed25519, UUIDv7 ids) in the project database; undo restores pointers |

States are folders: `layr show <state>:<path>` and `layr grep <pattern> <state>` read them
without a checkout.

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `group`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `checks`, `gc`, `fsck`, `reindex`           |
| remote (git)  | `remote`, `fetch`, `pull`, `push`, `clone`                                                    |
| machines      | `sync -- <cmd>` / `serve-peer`, `machines [trust]`, `op export` / `op import` (JSONL), `backup` / `backup restore` / `backup verify` |
| copies        | `export <folder>`, `slot sync|collect|status|forget` (host_exec slots on the Mac)              |
| setup         | `init`, `projects`, `config`, `layer add|remove|list`, `daemon`                               |

Revisions: `HEAD`, `@`, `HEAD~2`, `HEAD^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids of imported or pushed states, `stash@{n}`, `HEAD@{n}`
(automatic states, newest first).

## Agent workflow

1. The lead gives each worker a line: `layr branch w1 --owner agent1`.
2. The worker commits in its folder. Studio calls `layr save --turn-end` after each turn.
3. Review takes an exact state: `layr diff main...w1`, `layr try <state> -- <tests>` runs the
   command in a temporary copy of that state and records the result.
4. `layr merge w1 --expect <state>` merges exactly the reviewed state and refuses if the line
   moved. A merge never stops on a conflict: the new state lists the conflicts, text files get
   markers, and `layr status` shows them.
5. `layr push origin <task-branch>` exports the head as one git commit. The exact commit id is
   recorded before the push; after a lost response, a repeated push reads the remote ref first.
   With `push.require_check true`, a push needs a passing `layr try` of that exact state.

## Merge engine

Change lists of both sides come from `btrfs send --no-data` against the common base. A path
changed on one side takes that side (a reflink copy). Tree rules cover moves (inode level),
folder deletes against new files, links, executable bits, file/folder type changes, add/add,
rename/rename and delete/modify. Files changed on both sides use the driver from
`.gitattributes` or `.layrattributes`:

| driver            | behavior                                                                          |
| ----------------- | --------------------------------------------------------------------------------- |
| `text` (default)  | three-way line merge (`git merge-file`)                                           |
| `union`, `ours`, `theirs`, `binary` | as in git                                                       |
| `blocks`          | binary merge by byte ranges from the change lists: allowed when both sides kept the inode and size and the ranges do not overlap; `merge-verify=<name>` runs a check command |
| `<name>`          | `merge.driver.<name>` command with `%O %A %B %P`, run as the lead of this machine   |

## Storage and consistency

```text
$LAYR_ROOT/machine/{id,name,key}       machine identity, Ed25519 key (0700)
$LAYR_ROOT/projects/<p>/
  states/<state-id>                    read-only snapshots (0700)
  lines/<line>                         working folders (0711; each line 0700, its owner)
  git/                                 git object store for the remote bridge
  meta/db/layr.sqlite                  signed records + derived tables (own subvolume, 0700)
  meta/stage, meta/scan, meta/work     staging lines and temporary snapshots
```

An operation writes its snapshots first, then one SQLite transaction (synchronous=FULL) with
its record and the derived rows. After a crash only a snapshot without a record can exist;
`layr fsck --repair` deletes it. Derived rows keep the order key of their record, so a rebuild
(`layr reindex`) gives the same result as incremental imports.

## Security model

- The service runs as root; every other user is a client (`SO_PEERCRED`). Agents cannot read
  `states/` or the database; they read history through layr.
- A line belongs to its owner. All writes of the service into a line go through descriptors
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`); a link in a line never redirects a
  root write. New working folders are prepared in a private folder and appear when ready.
- Commands for users (git network access, `try`, `sync`, merge drivers, `apply` in a line) run
  with that user's uid, groups and a clean environment. Paths named by a user outside the store
  (export, slots, backups, patch files) are accessed with the user's file permissions.
- Undo, merge, push, tags and groups check line owners and the project lead. Only root changes
  the lead or makes root-owned lines.
- Replication takes records only from trusted machines (`layr sync` trusts the peer the lead
  chose; `layr machines trust`), checks the project id, signatures and sequence numbers, and
  refuses a changed copy of a known record. Lead, members, merge commands, remotes and the
  backup folder are per machine; other machines' records never change a local line.
- The service limits running requests per user and in total, and drops a client that does not
  send its request within 10 seconds.

## Backups and replication

`layr backup <folder>` writes the whole project: one btrfs stream per state (incremental),
git bundles, and all records as JSONL, with a SHA-256 manifest, written as the calling user.
`layr backup restore` checks the manifest, receives the streams, imports the records, and gives
the lines working folders on the new machine. `layr sync -- ssh host layr serve-peer <p>`
exchanges records both ways and sends missing states with `btrfs send -p <common state>`;
`--auto --layers` also carries working content and build output for a live move.

## Performance

One run in a Linux VM (OrbStack, 17 vCPU, kernel 7.0), release build, btrfs on a loop file.
Project: 200,000 small files and one 100 MiB file. The second column follows a write of 300 MiB
of ignored build output, so btrfs is still flushing; the third is a quiet tree.

| Operation                                | after a large write | quiet  |
| ---------------------------------------- | ------------------: | -----: |
| `status`, clean                          | 0.02 s              | 0.02 s |
| `status`, 101 edits                      | 0.80 s              | 0.05 s |
| `diff --stat`                            | 0.24 s              |        |
| `add -A`                                 | 0.68 s              |        |
| `commit`                                 | 1.48 s              | 0.35 to 0.5 s |
| `branch` (a new line, any size)          | 0.18 s              | 0.28 s |
| `merge` (50 + 50 changed files)          | 1.35 s              |        |
| `log -5`, `show --stat`                  | 0.003 s, 0.014 s    |        |
| `git status` of the same tree, for scale | 0.14 s              |        |

Most of the time is btrfs transaction commits: every snapshot commits one (15 to 600 ms, more
while dirty data is being written). The database lives in its own subvolume: an fsync there
after a snapshot takes about 6 ms, against about 100 ms in the subvolume that holds the
snapshot folders.

## Not in this version

- APFS backend (macOS projects outside the VM).
- The host_exec channel itself and the read-only network share are Studio features; layr
  provides the slot copies (`layr slot`).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Layer snapshots are checked the same way through the signed `save.layers` record.
