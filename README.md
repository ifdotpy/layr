# layr

Local version control on btrfs, for many users and many working folders on one machine. Git and GitHub stay the remote
(push, pull requests, CI). Everything local is btrfs: each line (branch and worktree in one) is a
writable snapshot, each state (commit) is a read-only snapshot, change lists come from
the btrfs metadata-send ioctl in O(changes), and a merge copies files with reflinks.

The commands follow the git verbs and print git formats, so people and tools that know git use
layr without new habits. Output of `diff`, `status --porcelain`, `show`, `log --format` and `--stat`
matches git byte for byte for the same content (tests/t08-gitcompat.sh).

Supported backend: Linux (btrfs, kernel 5.6 or newer). The experimental APFS backend is described below. Unsafe code is limited
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

## Experimental OverlayFS backend

The Linux `overlayfs` feature adds a backend for a VM with ext4 or XFS and root mount permission.
See [the setup, transfer rules and limits](docs/overlayfs.md).
The default Linux build remains btrfs.
Working caches are ordinary directories, so tools can remove and recreate them with `rm -rf`.
Snapshots copy the upper layer and share unchanged lower layers.

### OverlayFS benchmark results

Measured on 2026-10-16 with release binaries in one Linux VM: 10,000 files, 10 edits, three trials per binary.
The comparison uses the previous full-tree implementation and the new delta snapshots. Times are medians.
Commands run as root without the service; these tests do not measure changes of branch ownership.

| Filesystem | Operation | Full-tree snapshots | Delta snapshots | Speedup |
| --- | --- | ---: | ---: | ---: |
| ext4 | Clean status | 1069 ms | 395 ms | 2.7x |
| ext4 | Diff | 1095 ms | 276 ms | 4.0x |
| ext4 | Commit | 945 ms | 438 ms | 2.2x |
| ext4 | Merge | 3145 ms | 867 ms | 3.6x |
| XFS | Commit | 4675 ms | 2125 ms | 2.2x |
| XFS | Merge | 13279 ms | 3939 ms | 3.4x |

- ext4 private file allocation falls by 49.8%, from 492,957,696 to 247,443,456 bytes across three benchmark projects.
  This excludes directory and filesystem metadata. It is not the cost of one commit.
- XFS uses alternating runs. Its median `init` time rises from 10.3 to 19.1 seconds, an unresolved regression.
- VM latency varies greatly. Do not compare absolute ext4 and XFS times or treat these results as fixed backend limits.
- XFS can share reflink extents. Its file block counts do not establish physical space savings.
- Hard-linked trees and layer compaction still require full copies. The upper layer can contain changes from several commits.

See the [full report and raw samples](benchmarks/overlayfs-delta-20261016/README.md) for commands, hashes, all results and test coverage.

### Disk use compared with Git worktrees

**16 branches with private caches. About 94% less disk space.**
Layr on btrfs: **153 MiB**. Git worktrees on btrfs with full cache copies: **2508 MiB**.
These totals include the main workspace. The measured fixture uses 128 MiB of cache data per workspace.

Two runs use the same synthetic project: 2,048 source files, one main workspace, and 16 branches with one edit and commit each.
The cache case adds 128 MiB of private mutable data per workspace. Totals include history and additional filesystem metadata.

| Scenario | Layr | Git worktree |
| --- | ---: | ---: |
| Source only, btrfs | 22 MiB | 325 MiB |
| Source only, OverlayFS on ext4 versus Git on ext4 | 33 MiB | 283 MiB |
| Source only, OverlayFS on XFS versus Git on XFS | 19 MiB | 301 MiB |
| Private caches, btrfs, full Git cache copies | 153 MiB | 2508 MiB |
| Private caches, btrfs, reflink Git cache copies | 153 MiB | 455 MiB |

Layr on btrfs uses about 94% less space than Git with full cache copies in this fixture.
With reflink cache copies, the reduction is about 66%.
Git worktree does not populate ignored caches itself; the benchmark copies them explicitly.
Shared package-manager caches can change the result.

The current OverlayFS cache path copies data even on XFS: 2324 MiB versus 432 MiB for Git with reflink cache copies.
The source-only result does not establish the same benefit for private caches.
See the [method, full comparison and both raw runs](benchmarks/workspace-disk-20261016/README.md).

## Experimental APFS backend

APFS uses whole-directory clones automatically. There is no mode flag.
Trees with hard links or nested layers use file-by-file clones to preserve their behavior.
See the [10,000-file measurements](benchmarks/apfs-performance-20261015.md) for time and space costs.


The macOS build uses APFS. The Linux build continues to use btrfs with the existing store and record formats.
The store and signed record formats remain compatible with the Linux backend.

- The store requires a case-sensitive APFS volume.
- Commands run as the current user by default. Set `LAYR_SOCKET` to use the root service.
- The service requires a volume with ownership enabled and a separate root-owned store.
  It refuses volumes mounted with `noowners` and store folders that other users can change.
- A snapshot clones each file and checks source metadata before and after the copy.
  It fails if the source changes during the copy. It is not an atomic volume snapshot.
- APFS work grows with the number of files. Files with the same valid APFS clone identity need no content comparison.
  Other files require a content comparison to find changed blocks.
- `status` checks kernel change counters before and after a live read. It uses a temporary snapshot if validation fails.
  Files with an active memory mapping or an unavailable counter also use the snapshot path.
- `add` clones changed files into the private index instead of copying all bytes. Hard-linked index files use the existing copy path.
- `add` validates the working tree before and after it copies selected entries. It avoids full temporary snapshots when validation succeeds.
  The first `add` still creates a complete index from the head. Active `mmap` files use the snapshot path.
- A regular `commit` moves the prepared index into the state store and makes it immutable.
  A journal restores the index after a process stops before the database transaction completes.
  Commits with overlays retain the existing path.
- File modes, symbolic links, hard links and file identity survive snapshots. Nested layers stay outside history.
- Immutable flags prevent ordinary writes to states. The store owner can remove these flags.
  Use the root service for separate users and root-owned state storage.
- Overlays, `sync`, `serve-peer`, backup and restore are available.
- APFS sends full btrfs version 1 streams. Existing Linux peers can receive them without an update.
- APFS receives full and incremental streams. It copies clone ranges when APFS cannot share those ranges.
- APFS does not request compressed version 2 streams. Transfers between Linux peers retain their existing optimizations.
- A local restore gives the current user the admin role on the restored project.
- Transfers preserve file modes, hard links, symbolic links and extended attributes.
  macOS attribute names use a reserved `user.layr.apfs.*` namespace on Linux.
  Attributes of symbolic links use `trusted.layr.apfs.*`, because Linux forbids `user.*` attributes on symbolic links.
- Native access control lists cannot use the same rules on both systems.
  A transfer refuses macOS access control lists and Linux security attributes instead of silently losing their effects.
- Device nodes, sockets and named pipes are not supported by the APFS backend.
- APFS copies directory metadata, including access control lists and extended attributes.

Use a separate test store on an existing case-sensitive APFS volume:

```sh
cargo build --release
export LAYR_ROOT=/Volumes/YourCaseSensitiveVolume/layr-test-store
./target/release/layr init app --from ~/src/app
./target/release/layr --project app --line main branch topic
```

The core command tests also run on macOS. They require GNU coreutils and GNU sed in `PATH`.
`LAYR_TEST_DIRECT_MAIN=1` removes the default `main` policy in test projects.
The Linux tests use root and already bypass that policy. This option does not test access control.

```sh
export LAYR_APFS_TEST_ROOT=/Volumes/YourCaseSensitiveVolume
LAYR_TEST_DIRECT_MAIN=1 tests/run.sh t01-basic t02-merge t03-git t08-gitcompat
cargo test -- --ignored
```

The benchmark creates a new test directory and keeps its data and `results.json` for inspection:

```sh
python3 benchmarks/backends.py --binary target/release/layr \
  --root /Volumes/YourCaseSensitiveVolume/layr-bench-1000 --files 1000 --repeats 3
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
| cache     | a folder of regenerable content (such as a Rust `target`) kept as a nested subvolume: outside states, diffs and backups. A new line starts with a reflink copy of the nearest ancestor's cache (another user's only from a protected line; only one saved on the same platform, `<os>-<arch>`). Caches come only from `layr save --caches`; layr never keeps build output by itself. Cache folders: `layr cache add` and the folders that `.layrattributes` files mark `regenerable`; a folder with files in the head state is no cache |
| session   | a free-text label of the program that runs layr (`LAYR_SESSION`): shown in `op log` and used by `undo` to find "my last operation"; layr never decides permissions by it |
| record    | one signed operation (Ed25519, UUIDv7 ids) in the project database; undo restores pointers |
| role      | reader, writer, maintainer or admin of a user, a Unix group or everyone, per project and machine |
| protected line | a line (pattern) that changes only through merges of reviewed states, after its checks pass on the merge result |
| overlay   | files with their own owner, readers and history, shown in the lines of their readers above the shared set; `push`, exports, backups and every state of the project hold the shared set only (`docs/overlays.md`) |

States are folders: `layr show <state>:<path>` and `layr grep <pattern> <state>` read them
without a checkout.

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `attest <kind> <rev> [--payload <file>]`, `attestations <rev> [--kind <kind>] [--json]` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `gc`, `fsck`, `reindex`           |
| remote (git)  | `remote`, `fetch`, `pull`, `push [--squash]`, `clone`, `git-sync`                             |
| machines      | `sync -- <cmd>` / `serve-peer`, `machines [trust]`, `op export` / `op import` (JSONL), `backup` / `backup restore` / `backup verify` |
| copies        | `export <folder>`                                                                              |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow]`, `overlay list`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>`, `overlay move [--purge] <name> <paths>`, `overlay log\|show\|diff\|grep\|ls-files <name>` |
| refs          | `ref set <name> <rev> [--expect <rev>] [--ttl <dur>]`, `ref list [<prefix>]`, `ref log <name>`, `ref delete <name> [--expect <rev>]` |
| changes       | `change list`, `change show <id>`, `change log <id>`, `change diff <id> [--from vN] [--to vM]` |
| setup         | `init`, `projects`, `config`, `cache add|remove|list|repair`, `daemon`                        |

Revisions: `HEAD`, `@`, `HEAD~2`, `HEAD^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids of imported or pushed states, `stash@{n}`, `HEAD@{n}`
(automatic states, newest first).

Content ids: a state ID is the same only in copies that share its record, but the git ids of its
content are the same on every machine and in git. `layr rev-parse <rev>^{tree}` (and `%T`/`%t` in
`--format`) prints the tree of the commit that `layr push` exports for the state. `layr rev-parse
<rev>:<path>` prints the id of one path in that commit, as `git rev-parse` does: a tree for a
folder, a blob for a file, the root tree for `<rev>:`. `./` and `../` resolve against the current
folder. A path that the exported commit does not have fails with exit 128.


## Refs, changes, and attestations

Refs name states without a work folder, for example `env/prod` or `pins/build`.
Mutable ref names must start with `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, or `derived/`.
`ref set` refuses line names. Use `commit`, `merge`, or `reset` to change a line.
`--expect` compares the current state before the update. A mismatch changes nothing and returns an error.
`--ttl` accepts positive durations such as `30s`, `5m`, `2h`, and `1d`.
All ref and change commands accept `--json`.

```sh
layr ref set env/prod HEAD
layr ref set env/prod HEAD~1 --expect HEAD
layr rev-parse 'env/prod@{1}'
layr ref list env/ --json
layr ref log env/prod
layr undo
```

`<ref>@{0}` selects the current position. `<ref>@{1}` selects the previous position. `ref log` lists the newest position first.
`ref log` retains positions after delete. `undo` restores a ref if no later update changes it.
Garbage collection (GC) keeps live ref targets and their dependencies.
Expired refs stop resolving. GC records their removal. The normal undo window can retain earlier positions.

These revision prefixes are reserved: `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, `derived/`,
`changes/`, `views/`, and `attestations/`. Line names cannot start with these prefixes.
`views/<id>` returns an explicit error because views do not exist yet.

Each new source commit has a `change_id`. Amend, rebase, and cherry-pick preserve it.
A merge commit has a new identifier. Old states without `change_id` remain readable.
`changes/<id>` selects the latest version. `changes/<id>/v1` selects the first version.
`change show` lists versions and their attestations. `change log` lists their operations.
`change diff` compares each version's patch against its own parent, so unrelated base changes do not enter the interdiff.

Attestation kinds: `check`, `approval`, `origin`, `import`, `quarantine`, and `revoke`.
Each attestation is part of an immutable operation record with a machine signature.
It records the exact subject state, optional ChangeId, payload digest, signer, and time.
`--payload` reads a file as the caller and records its SHA-256 digest. It does not store the file contents.
The optional payload reference field is reserved for an object in an allowed layer.

`approve` writes an approval attestation. `approve --withdraw` appends a revoke attestation for the caller's approvals.
`attest revoke` revokes the caller's earlier attestations of the subject on this machine.
The original evidence remains in the log. A revoke cannot withdraw another caller's evidence.
`try` records a check attestation and its command result. Protected-line rules continue to use approvals and checks.
The old `approvals` and `checks` commands fail and name `layr attestations` as their replacement.
Existing signed approval and check records populate the new attestation table without changes to their signatures.
Attestations do not keep a subject alive. Their records remain available while the subject lives.

State kinds `cache` and `derived` have optional `derived_from` and `context` fields.
They have no source parents, line, or ChangeId. Lines, source diffs, merges, and Git exports reject them.
A live ref can keep an auxiliary state and its source alive.
Existing cache snapshots keep their attachment to source states and their wire format for older clients.
They remain cache data outside source diffs. This change does not convert those snapshots into separate StateIds.
The new kinds support separate cache and derived states through the store API.

The database schema version stays at 6. New tables are additive. Optional fields are absent from old state records.


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
the commit that `layr push` sends for that state, with the commit of the previous state as its
parent, and the index of that state. So `git rev-parse HEAD` changes only when the head moves,
`git status` shows only the uncommitted changes, and `layr rev-parse <commit>` finds the state of
such a commit. The commit is made in the local git store only; nothing leaves the machine before
a push. A push with `--squash` sends another commit for the same state; `HEAD` then shows that
commit. `layr git-sync` also records the commits of the head and its ancestors, as a push does.
A `.git` folder that the line owner made instead is left alone.

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
  meta/base/<line>@<key>               full tree of a line with overlays (derived)
  meta/overlays/<overlay>/             an overlay: the internal project <p>%<overlay>, same layout
```

An operation writes its snapshots first, then one SQLite transaction (synchronous=FULL) with
its record and the derived rows. After a crash only a snapshot without a record can exist;
`layr fsck --repair` deletes it. Derived rows keep the order key of their record, so a rebuild
(`layr reindex`) gives the same result as incremental imports.

## Security model

- The service runs as root; every other user is a client (`SO_PEERCRED`). Clients cannot read
  `states/` or the database; they read history through layr.
- The socket (`/run/layr/layr.sock`, mode 0666 in a folder of mode 0755) is open to every local
  user: the service decides each request by the caller's uid, groups and role, so a reader runs
  every read command. A connection refused with "Operation not permitted" (EPERM) never comes
  from layr: a sandbox or seccomp filter of the calling process refused the `connect()` system
  call (for example a filter that denies `connect` for every socket family when network access is
  off). Such a process needs `connect()` to the socket allowed, or must run layr outside that
  sandbox; the client error says so.
- The git object store (`git/`) holds history too. Its folder alone decides who reads it: open
  to the users and groups with a role and the owners of lines (an access list, or `0755` when
  everyone may read). git writes below it with `core.sharedRepository=0644`, so a strict umask
  of the service (such as `0077`) does not close the objects that the `.git` folder of a line
  uses through `alternates`. A store of an earlier layr gets read access below its folder once:
  at the next new line or role change, or at the first periodic run after the service starts.
- A line belongs to its owner. All writes of the service into a line go through descriptors
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`); a link in a line never redirects a
  root write. New working folders are prepared in a private folder and appear when ready.
- Commands for users (git network access, `try`, checks, `sync`, merge drivers, `apply` in a
  line) run with that user's uid, groups and a clean environment: a command never runs as
  another user. Paths named by a user outside the store (export, backups, patch files) are accessed with the user's file permissions, and so
  is a file such a command leaves for the service (the result of a merge driver).
- Overlays (docs/overlays.md): an overlay is read only through its own project, by its readers;
  the working content of a line with overlays is open to its owner and to readers of all its
  overlays; automatic states leave overlay files out, so no state of the project holds one.
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
  project, and clone sources limited to that parent, the new subvolume, or explicitly verified states of the same project.
  Extra sources require an exact stream UUID and transaction ID. Their file paths cannot traverse symbolic links. After the receive, the new subvolume must be a snapshot of exactly that parent.
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

## Btrfs execution and sync

Metadata comparisons call the kernel directly. They do not start a `btrfs send` process.
Comparisons of immutable state pairs can reuse a private cache in `meta/diff-cache`.
The cache holds at most 256 entries and 32 MiB of file data per project.
Each entry contains at most 8 MiB, including its checksum.
New snapshots and temporary scans do not enter this cache during the command that creates them.
A missing or damaged entry causes a fresh kernel comparison. Cache writes do not require an fsync.
The cache is disposable and is not part of records or backups.

Sync peers exchange capability fields in their initial message:

- Peers that support extra clone sources can reuse data from up to eight additional shared states.
  The sender examines at most 32 recent candidates for each state.
  Peers exchange source identities and use ordinary data transfer when transaction identities do not match.
  The receiver checks each source against a signed local state, its UUID and its read-only flag.
  Received UUIDs must also match their recorded send transaction; original local UUIDs identify the source directly.
  Reuse of original local UUIDs requires btrfs-progs 6.11 or later. Older receivers get file data for those sources.
- Peers with compatible btrfs tools can exchange compressed extents with stream protocol 2.
  The sender also checks kernel support. This preserves existing compression; it does not compress uncompressed files.
- A peer without these capabilities gets the previous stream and clone rules.
  Backups retain protocol 1 and the previous manifest format.

Working content still requires an atomic snapshot before comparison.
The optimization does not replace this snapshot with an unsafe check of a writable tree.
It does not enable filesystem compression or run defragmentation automatically.
Defragmentation can break shared extents and increase disk use, as described in the
[btrfs documentation](https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html#subcommand).
Protocol requirements are described in [btrfs-send](https://btrfs.readthedocs.io/en/latest/btrfs-send.html).

Reproduce the measurements on Linux with root access:

```sh
python3 benchmarks/btrfs-optimizations.py --before /path/to/old/layr --after target/release/layr \
  --root /new/path/on/btrfs --files 10000 --repeats 12
bash benchmarks/sync-storage.sh /path/to/old/layr target/release/layr /tmp/sync-results.json
OLD_LAYR=/path/to/old/layr LAYR=$PWD/target/release/layr bash tests/compat-btrfs.sh
```

The compatibility test covers state IDs, signatures, backups in both directions, and both sync roles with an older binary.
It also checks saved cache content across old and new clients.
The CLI uses `cache` and `--caches`; `layer` and `--layers` remain aliases.
Cache records retain the legacy `save.layers` name and `layers` fields on disk.
The reader also accepts the intermediate `save.caches` format.
Sync selects the cache message name from the peer capabilities.

### Measured btrfs optimization results

Measured on 2026-10-14 in the OrbStack Linux VM, with release binaries and 10,000 files of 4 KiB.
The final test uses a separate 8 GiB loop filesystem and 12 pairs of commands with alternate execution order.
The table shows median elapsed time, including command startup. The immutable diff uses a warm cache.

| Operation | Before | After |
| --- | ---: | ---: |
| Clean status | 21.36 ms | 14.42 ms |
| Repeated diff between states | 8.33 ms | 3.63 ms |
| Clean save | 18.98 ms | 14.46 ms |

The first diff with a cache miss takes 10.12 ms in this run.
Earlier tests on the shared filesystem sometimes show slower status and save commands after the change.
These results do not establish a general elapsed-time improvement for those commands.
Repeated immutable diffs improve in every recorded run. Unrelated host activity was not controlled.

Separate sync tests check exact file content and allocated extents after a filesystem sync:

| Fixture | Stream bytes before | Stream bytes after | Reduction |
| --- | ---: | ---: | ---: |
| A 16 MiB file removed, then restored through a reflink | 33,579,189 | 16,783,017 | 50.0% |
| An 18,432,000-byte text file already compressed with zstd | 18,449,311 | 590,689 | 96.8% |

The restored file shares all 16 MiB of its data extents after sync. Previously, it had a separate 16 MiB allocation.
This space comparison excludes filesystem metadata. Compression savings depend on the source data and its existing compression.

Raw results contain binary hashes and individual samples:
[final timing test](benchmarks/results/btrfs-opt-isolated-10000.json),
[sync and extent checks](benchmarks/results/btrfs-opt-sync.json),
[initial timing test](benchmarks/results/btrfs-opt-initial-10000.json),
[shared filesystem repeat](benchmarks/results/btrfs-opt-shared-repeat-10000.json),
and [shared filesystem CPU measurements](benchmarks/results/btrfs-opt-shared-cpu-10000.json).
The earlier runs use intermediate builds; their hashes identify each build.

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

### Experimental APFS comparison (provisional)

Measured on 2026-10-14 with the implementation in `ada0cef`.
Each fixture contains 1,000 or 10,000 files of 4 KiB. Ten files receive an extra line.
The source content hashes match across both backends. Each operation has three trials.
The table gives median seconds, including command startup.

APFS runs natively on macOS 27.2, on a case-sensitive volume in an ASIF image.
btrfs runs in the OrbStack Linux VM on the same Mac.
Those initial backend runs were sequential. The host also ran other tasks. Their load was not controlled.

| Operation | btrfs, 1,000 files | APFS, 1,000 files | btrfs, 10,000 files | APFS, 10,000 files |
| --- | ---: | ---: | ---: | ---: |
| init | 0.354 | 1.040 | 0.509 | 18.289 |
| branch | 0.035 | 0.285 | 0.032 | 3.618 |
| status, clean | 0.016 | 0.674 | 0.014 | 7.758 |
| diff, 10 edits | 0.021 | 0.627 | 0.019 | 7.766 |
| add -A | 0.028 | 0.897 | 0.024 | 12.314 |
| commit | 0.030 | 0.534 | 0.025 | 8.849 |
| merge, fast-forward | 0.043 | 1.350 | 0.040 | 35.101 |

These values are provisional. They do not establish an isolated filesystem speed ratio.
A later test alternates clean status commands on both backends with 10,000 files.
It measures APFS at 11.00 to 13.26 seconds and btrfs at 0.09 to 16.33 seconds.
The host's one-minute load average ranges from 294 to 311 during that test.
A further benchmark found no quiet interval within 30 minutes. The comparison remains provisional.

The measured baseline clones every file for a snapshot and scans content for changed blocks.
The btrfs implementation uses subvolume snapshots and send-stream change lists.
Those mechanisms have different costs as the file count grows.
The test image still has 96% free space after the runs. Spotlight indexing is disabled on it.

Raw data includes the binary hash and source content hash:
[APFS, 1,000 files](benchmarks/results/apfs-1000.json),
[APFS, 10,000 files](benchmarks/results/apfs-10000.json),
[btrfs, 1,000 files](benchmarks/results/btrfs-1000.json),
[btrfs, 10,000 files](benchmarks/results/btrfs-10000.json),
and [interleaved status](benchmarks/results/paired-status-10000.json).

At the benchmark revision, the previous btrfs binary (`c6063e4`) and the candidate passed thirteen compatibility checks.
Both binaries read each other's states, verify signatures and restore each other's backups without changing state identifiers.
Run those checks with `OLD_LAYR=/path/to/old/layr LAYR=/path/to/new/layr bash tests/compat-btrfs.sh` on Linux as root.

A later APFS run includes overlays and the compatible transfer implementation.
Three trials give these median times:

| Operation | 1,000 files | 10,000 files |
| --- | ---: | ---: |
| branch | 0.388 s | 4.058 s |
| status, clean | 1.074 s | 9.264 s |
| commit | 0.850 s | 8.666 s |
| merge, fast-forward | 2.401 s | 22.721 s |

The host had other work, so these results remain provisional.
The Linux comparison could not start because the OrbStack executable was absent.
These results do not establish a speed ratio against btrfs.
The measured binary predates the final service permission checks.
Raw data: [1,000 files](benchmarks/results/apfs-complete-1000.json)
and [10,000 files](benchmarks/results/apfs-complete-10000.json).

## Not in this version

- APFS volume snapshots and incremental APFS sends. Current snapshots use verified directory clones; APFS sends use full streams.
- Overlays beyond the first slice: routing patterns, ephemeral mode, folder overlays, encrypted
  backups and replication of overlays (`docs/overlays.md`, section 9.7).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Cache snapshots are checked the same way through the signed `save.caches` record.

### APFS service test

The service uses kernel peer credentials and runs each request in a separate process.
The client supplies file descriptors for its current folder and standard streams.
The service changes credentials before it accesses caller-owned paths or starts caller commands.

Enable ownership on the dedicated test volume before the service test:

```sh
sudo diskutil enableOwnership /Volumes/YourCaseSensitiveVolume
sudo env LAYR_APFS_TEST_ROOT=/Volumes/YourCaseSensitiveVolume \
  LAYR_APFS_TEST_USER="$(whoami)" LAYR="$PWD/target/debug/layr" \
  bash tests/apfs-service.sh
```

The test uses the current user and `nobody`. It creates no system accounts.
Use `tests/apfs-cross.py` to test sync and backup compatibility with a Linux binary in an OrbStack VM.
