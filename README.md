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

## Experimental APFS backend

The macOS build uses APFS. The Linux build continues to use btrfs with the existing store and record formats.
This branch does not yet provide full backend compatibility.

- The store requires a case-sensitive APFS volume.
- Commands run as the current user. The root service is not available on macOS.
- A snapshot clones each file and checks source metadata before and after the copy.
  It fails if the source changes during the copy. It is not an atomic volume snapshot.
- APFS work grows with the number of files. A diff also reads file content to find changed blocks.
- File modes, symbolic links, hard links and file identity survive snapshots. Nested layers stay outside history.
- Immutable flags prevent ordinary writes to states. The store owner can remove these flags.
  This does not provide the Linux service's protection against other users.
- Native `sync`, `serve-peer` and `backup` are unavailable. The Git remote bridge works.
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
| cache     | a folder of regenerable content (such as a Rust `target`) kept as a nested subvolume: outside states, diffs and backups. A new line starts with a reflink copy of the nearest ancestor's cache (another user's only from a protected line). Caches come only from `layr save --caches`; layr never keeps build output by itself. Cache folders: `layr cache add` and the folders that `.layrattributes` files mark `regenerable`; a folder with files in the head state is no cache |
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
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `approvals <state>` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `checks`, `gc`, `fsck`, `reindex`           |
| remote (git)  | `remote`, `fetch`, `pull`, `push [--squash]`, `clone`                                         |
| machines      | `sync -- <cmd>` / `serve-peer`, `machines [trust]`, `op export` / `op import` (JSONL), `backup` / `backup restore` / `backup verify` |
| copies        | `export <folder>`                                                                              |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow]`, `overlay list`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>`, `overlay move [--purge] <name> <paths>`, `overlay log\|show\|diff\|grep\|ls-files <name>` |
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

The APFS implementation clones every file for a snapshot and scans content for changed blocks.
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

## Not in this version

- Complete APFS support: a privileged service, atomic snapshots, overlays and native state transfer.
- Overlays beyond the first slice: routing patterns, ephemeral mode, folder overlays, encrypted
  backups and replication of overlays (`docs/overlays.md`, section 9.7).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Cache snapshots are checked the same way through the signed `save.caches` record.
