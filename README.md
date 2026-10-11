# layr

Local version control on btrfs, for many users and many working folders on one machine. Git and GitHub stay the remote
(push, pull requests, CI). Everything local is btrfs: each line (branch and worktree in one) is a
writable snapshot, each state (commit) is a read-only snapshot, change lists come from
`btrfs send --no-data` in O(changes), and a merge copies files with reflinks.

The commands follow the git verbs and print git formats, so people and tools that know git use
layr without new habits. Output of `diff`, `status --porcelain`, `show`, `log --format` and `--stat`
matches git byte for byte for the same content (tests/t08-gitcompat.sh).

Status: working implementation, Linux only (btrfs, kernel 5.6 or newer). Unsafe code is limited
to `src/sys.rs` (btrfs ioctls, fork, user switching, the user database); everything else uses
`rustix` and the standard library, and the crate denies `unsafe_code` outside that module.

layr is a general tool: it knows Unix users, projects, lines, states and rules, and nothing about
the programs that use it. Programs built on top of it use only its public commands.

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
sudo layr init app --from ~/src/app --owner alice    # alice is the admin; main is protected
layr access grant bob writer                         # (alice) roles: reader, writer, maintainer, admin
layr branch topic-1 main                             # (bob) a new line: O(1), its own folder
cd /var/lib/layr/projects/app/lines/topic-1          # a line is a folder
layr status; layr diff; layr add -A; layr commit -m "change"
layr merge topic-1 --expect <state alice reviewed>   # (alice, in main) exactly the reviewed state
layr push origin main                                # every state of main becomes a git commit
```

The store lives under `$LAYR_ROOT` (default `/var/lib/layr`). To work on a line from another
path, link it: `ln -s /var/lib/layr/projects/app/lines/topic-1 ~/code/topic-1`. Users other than root talk to the
service; root can run every command directly.

## Concepts

| Term      | Meaning                                                                                     |
| --------- | ------------------------------------------------------------------------------------------- |
| state     | read-only snapshot + signed record (id, parents, author, message, conflicts). A commit.    |
| line      | writable snapshot at `projects/<p>/lines/<name>` with an owner and a head state             |
| auto state| a snapshot of the working content: before every changing command, on `layr save`, and every 10 minutes when the content changed (the service) |
| cache     | a folder of regenerable content (such as a Rust `target`) kept as a nested subvolume: outside states, diffs and backups. A new line starts with a reflink copy of the nearest ancestor's cache (another user's only from a protected line). Caches come only from `layr save --caches`; layr never keeps build output by itself. Cache folders: `layr cache add` and the folders that `.layrattributes` files mark `regenerable` |
| session   | a free-text label of the program that runs layr (`LAYR_SESSION`): shown in `op log` and used by `undo` to find "my last operation"; layr never decides permissions by it |
| record    | one signed operation (Ed25519, UUIDv7 ids) in the project database; undo restores pointers |
| role      | reader, writer, maintainer or admin of a user, a Unix group or everyone, per project and machine |
| protected line | a line (pattern) that changes only through merges of reviewed states, after its checks pass on the merge result |

States are folders: `layr show <state>:<path>` and `layr grep <pattern> <state>` read them
without a checkout.

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `approvals <state>` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `checks`, `gc`, `fsck`, `reindex`           |
| remote (git)  | `remote`, `fetch`, `pull`, `push [--squash]`, `clone`                                         |
| machines      | `sync -- <cmd>` / `serve-peer`, `machines [trust]`, `op export` / `op import` (JSONL), `backup` / `backup restore` / `backup verify` |
| copies        | `export <folder>`                                                                              |
| setup         | `init`, `projects`, `config`, `cache add|remove|list|repair`, `daemon`                        |

Revisions: `HEAD`, `@`, `HEAD~2`, `HEAD^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids of imported or pushed states, `stash@{n}`, `HEAD@{n}`
(automatic states, newest first).

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
5. `layr push origin <branch>` exports the line's history: each state the remote lacks becomes
   a git commit with the same parents. `--squash [-m <message>]` exports one commit on top of the
   last exported state. The exact commit id is recorded before the push; after a lost response,
   a repeated push reads the remote ref first. With `push.require_check true`, a push needs a
   passing `layr try` of that exact state.

The design and the reasons for it, including what layr leaves to programs built on it, are in
`docs/design.md`.

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
| `<name>`          | `merge.driver.<name>` command with `%O %A %B %P`, run as the caller of the merge     |

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

- The service runs as root; every other user is a client (`SO_PEERCRED`). Clients cannot read
  `states/` or the database; they read history through layr.
- A line belongs to its owner. All writes of the service into a line go through descriptors
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`); a link in a line never redirects a
  root write. New working folders are prepared in a private folder and appear when ready.
- Commands for users (git network access, `try`, checks, `sync`, merge drivers, `apply` in a
  line) run with that user's uid, groups and a clean environment: a command never runs as
  another user. Paths named by a user outside the store (export, backups, patch files) are accessed with the user's file permissions, and so
  is a file such a command leaves for the service (the result of a merge driver).
- Permissions (docs/design.md, section 3): roles per project and machine (`layr access`), deny
  rules that win over any role, owners of lines, and protection rules of line patterns
  (`layr protect`). A new project has one admin (its creator) and a protected `main`. Stores of
  older versions keep their `admin` (or `lead`) and `members` settings as roles until `access`
  is set. Root may do everything, and every record names who acted.
- Replication takes records only from trusted machines (`layr sync` trusts the peer the admin
  chose once its records passed the checks; `layr machines trust`), checks the project id,
  signatures and sequence numbers, and refuses a changed copy of a known record. Roles, deny
  and protection rules, merge commands, remotes and the backup folder are per machine; other
  machines' records never change a local line.
- A btrfs stream from a peer or a backup passes a check on its way to `btrfs receive`, which
  would otherwise find snapshot and clone sources by UUID anywhere on the file system: one
  subvolume with the name and UUID its signed record names, a parent only from the same
  project, clones only from that parent or the new subvolume, and no path that leaves the
  subvolume. After the receive, the new subvolume must be a snapshot of exactly that parent.
- Files larger than 64 MiB count as binary for diff, grep, blame, rename detection and merge:
  the service never reads such a file into memory (`show` and merge drivers copy it in
  pieces). Patches, messages and records named by a user are limited in size too.
- The service limits running requests per user and in total, and drops a client that does not
  send its request within 10 seconds.

## Backups and replication

`layr backup <folder>` writes the whole project: one btrfs stream per state (incremental),
git bundles, and all records as JSONL, written as the calling user. The manifest lists the
SHA-256 of the bytes the service produced and is signed with the machine key, so a changed
file or manifest fails the check (a chain that no longer verifies is not continued; the next
backup starts a new chain). `layr backup restore` checks the signature with the key that the
backup's records give for the machine that wrote it (this machine's own key when it names this
machine), imports the records, receives each stream through the stream check, compares each
file with its SHA-256 while reading it once (no links, pipes or devices), and gives the lines
working folders on the new machine. A restore trusts the machine that signed the backup: a
backup made entirely by another machine identity restores as that machine's project.

`layr sync -- ssh host layr serve-peer <p>` exchanges records both ways and sends missing states with `btrfs send -p <common state>`;
`--auto --caches` also carries working content and build output for a live move.

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

- APFS backend (macOS).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Cache snapshots are checked the same way through the signed `save.caches` record.
