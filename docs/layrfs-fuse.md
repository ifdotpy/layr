# LayrFS FUSE view on Linux (experimental, measured)

Question: can a FUSE view of a line, served by a normal user over store v2 objects, replace root
on Linux (no Btrfs subvolumes, no `btrfs send`, no OverlayFS mounts, no `chown`)? What does it
cost against the Btrfs and OverlayFS backends?

Answer: yes. Line operations and saves do not need root, and they are as fast or faster. Agent
tools do not walk through the mount: `layr tool rg|find` answers from the view's own index, and
git asks the view's change list through its fsmonitor hook. Then only content searches stay
slower than on Btrfs (3.6 times on the Linux kernel), and first writes of many small files
(about 0.6 ms more per file). Builds are not slower when `target/` stays a native folder.

These measurements use the standalone `layr fs view` prototype in a Layr build with the
Linux Cargo feature `layrfs-fuse`. The prototype used one server process per line and ran
through the scripts in `tools/layrfs-fuse`. For the current service and `layr line new --fast`,
see [Fast lines](fast-lines.md).

## Prototype design

One server process per line, run by the service user `layr` (not root):

| Part | Where | Notes |
| --- | --- | --- |
| Lower | the tree and times objects of a saved v2 state (docs/objects.md) | folders load at first use; reads open the hot blob (`blobs/`), a cold blob is decoded once into `cold/` |
| Upper | `lines/<line>/upper/<n>`, one native file per changed file | a first write clones the blob (`FICLONE`); a rename does not move the file |
| Metadata | the memory of the server | names, modes, times, removals; each change appends one line to `journal` |
| Save | the server's change marks | blobs of the changed files, trees and times tables of the folders on the way to them; no scan, no `btrfs send`; the saved files leave `upper/` |
| Caches | a link `target -> <native folder>` at the root | made by the agent, owned by the agent, outside the mount; status and save leave it out |
| Index | the LayrFS index of the mount (docs/layrfs.md) | built at the mount in the background; `layr tool rg` reads the backing files, never the mount |
| Control | a Unix socket | `tool`, `fsmonitor`, `status`, `save`, `stats`, `reset`, `stop`; the caller's uid comes from `SO_PEERCRED` |

```sh
layr fs view start --objects <dir> --state <id> --line <dir> --mnt <dir> --sock <path> \
    [--cache target=<folder>] [--allow-other --allow <uid>] [--owner <uid>]
layr fs view ctl <sock> status|save <state>|stats|reset|stop
git config core.fsmonitor "layr fs view fsmonitor"; git config core.untrackedCache true
```

### Agent tools without walks through the mount

- **`layr tool rg|find`.** The mount's source name is `layrfs-view:<socket>`. In a view, the
  client finds the socket in `/proc/self/mountinfo`. It talks to the socket only when the
  process behind it runs as the mount's owner (`user_id=`), the user that serves every byte of
  the view anyway. The server answers with the LayrFS rules: the same output as the real tool,
  or a fallback, and then the real tool runs. The index is a walk of the mount, so it has the
  files and ignore rules that rg sees. A search reads the upper file or the blob of each file,
  never the mount. Before an answer, the server flushes the kernel's writeback cache (`syncfs`)
  and compares the folder times in the query's scope. The kernel answers these from its
  attribute cache, except for folders that changed. The server cannot read the agent's global
  git excludes, so the client runs the real tool when the agent has them.
- **git fsmonitor (hook version 2).** `layr fs view fsmonitor` gives git a token and the paths
  that changed since git's last token. The server keeps a change sequence and the last change
  of each node. It also keeps the paths that left the tree (removed or renamed away). Files open
  for writing are always listed, because the kernel's writeback cache holds their newest bytes.
  A token of another server instance gives `/` (everything): this happens after a restart,
  after a journal replay and after a reset. Then git scans everything once.

### Kernel caches

| Setting | Effect (request counters of `ctl stats`) |
| --- | --- |
| entry and attribute timeout 3600 s, negative entries | a second `find` over 1,860 files sends no request |
| `FOPEN_KEEP_CACHE`, writeback cache, readahead 1 MiB | a second `cat` of a file sends an `open` and no `read` |
| `opendir` answers `ENOSYS` | the kernel opens folders itself, keeps their listings (`FOPEN_CACHE_DIR`) and sends no `opendir` and no `releasedir` |
| readdirplus, parallel folder operations, 8 threads | a first walk gets names and attributes in one request per folder |

Every change through the mount goes through the kernel, so its caches stay valid. A change that
the server makes itself must invalidate them. `ctl reset` (drop every unsaved change) calls
`inval_entry` for every name of a loaded folder and `inval_inode` for every node. A save changes
no content (an upper file becomes the equal blob), so it invalidates nothing. `check.sh` checks
both cases. The earlier `--no-open` mode is gone: it gave no `open` events, so the fsmonitor list
could miss a file that a process was still writing.

A dead server leaves its mount, and for the entry timeout `stat` and `ls` can still answer from
the caches. `statfs` always asks the server, so a start uses it to find and remove a dead mount.

### Upper folder or memory

Changed data goes to native files, not to memory. Reasons:

- The kernel page cache holds the data. A copy in the server's memory would double it, and a
  crash would lose it unless a journal held the data too.
- A first write clones the blob, so a large file costs no copy on btrfs or XFS.
- A save hashes the upper files in parallel and clones them into `blobs/`.

Only metadata is in memory, with a journal. The journal is append-only and has no `fsync`.

### What a crash loses

- **Server crash (`kill -9`):** the mount gives `ENOTCONN` for every call that its caches do not
  answer. The next start removes the dead mount and replays the journal (`check.sh` tests 2 and
  5). A file that was closed before the crash keeps all its data: `close` flushes the writeback
  cache to the server. Dirty pages of a file that is still open are lost, because the kernel
  cannot write them to a dead server.
- **Power loss:** the journal and the upper files have the durability of normal files without
  `fsync`. A saved state has the durability of the object store's writes. A save `fsync`s the
  new journal.

### Tamper model

The store (`/home/layr/store`, mode 0700) belongs to the user `layr`. The agent user cannot read
it, cannot change a saved state, and cannot change the upper files. The agent uses the mount
through `allow_other`. The view shows every file as owned by the agent (`--owner`) and by the
server's group, so the kernel checks permissions as for the agent's own files
(`default_permissions`). The server refuses every other uid, in the mount and on the socket.

With long kernel caches, a request of another user can be answered from the caches and never
reach the server's uid check. So the root of the view shows mode 0750: the kernel stops other
users at the root. A tree stores no mode of its root, so every saved mode stays real (masking the
"other" bits of every file was lossy: `chmod u+x` wrote the masked mode back). The server reads
its own mount (the index) through the group bits. `check.sh` test 4 checks this. The view
replaces the `chown` of line folders: ownership is a property of the view, not of files on
disk.

Lab experiments (one server for many lines, states per step, rebase, lazy blobs, build traces,
passthrough): [layrfs-lab.md](layrfs-lab.md).

## Version 2 (2026-11-16)

Goal: be as fast as Btrfs or faster in every step of the benchmark. Run the view with
`--no-open --uring`. Raw data: [benchmarks/layrfs-fuse-20261116](../benchmarks/layrfs-fuse-20261116/README.md).

### What changed

| Change | Effect |
| --- | --- |
| `--no-open`: `open` answers `ENOSYS`, no writeback cache | the kernel opens files itself and keeps their pages; a warm `rg` through the mount sends no request. Every write reaches the server at once, so the fsmonitor list stays exact without open handles |
| `--uring`: FUSE over io_uring (kernel 6.14+, `fuse.enable_uring=1`, vendored `fuser` patch `vendor/fuser/src/uring.rs`) | one ring thread per CPU, pinned. A request is served on the CPU of its caller: no wakeup crosses CPUs, which costs much in a virtual machine. One append round trip: 35 to 45 us against 100 to 140 us |
| patches: a small write to a file without an upper copy is kept in memory and in the journal (`W` line); a new file has no upper file (`N` line) | a first write costs no file creation and no copy. A save stores the result from memory (`Store::put_hot_bytes`); `layr tool` and a large write make the upper copy first |
| git index stat data: the view reads `.git/index` and shows its inode number, group and change time for each file whose size and time match (vendored `fuser` patch `FileAttr::st_ino`) | git does not hash the files of a new line again: cold `git status` 0.9 s against 19.8 s |
| the index of `layr tool` takes the entries and times from memory (folders load in parallel); only ripgrep's walk reads the mount | index ready 0.5 s against 1.0 s |
| `layr tool rg`: the backing paths are found in parallel, under a shared lock (`RwLock` instead of `Mutex` for the whole view) | 0.19 s against 0.55 s before |
| `getxattr` answers `ENOSYS` when no file has extended attributes; `FUSE_HANDLE_KILLPRIV_V2` | no `security.capability` round trip before the first write of each file |
| `fs view start` forks the server and makes the mount folder; `ctl stop remove` unmounts, removes the line folder and answers after the unmount | line new and line delete are one command each, as with Btrfs |

`bench.sh` changes: the FUSE line commands are the two commands above (before: a shell with
`mkdir`, `sleep` loops and `rm`). The agent's cache folder of a FUSE line is made and removed
outside the timed steps (a Btrfs line has none).

### Results (median of 3 rounds, ms; load average 15 to 22)

Linux kernel:

| Step | FUSE | Btrfs | FUSE / Btrfs |
| --- | ---: | ---: | ---: |
| line new | 30 | 23 | 1.30 |
| line delete (clean) | 12 | 14 | 0.86 |
| tool index ready | 497 | 1,003 | 0.50 |
| `git status` (cold) | 920 | 19,789 | 0.05 |
| `git status` (warm) | 60 | 172 | 0.35 |
| `rg -l TODO` through the mount (first) | 1,381 | 257 | 5.37 |
| `rg -l TODO` through the mount (warm) | 236 | 217 | 1.09 |
| `layr tool rg -l TODO` (warm) | 192 | 158 | 1.22 |
| `find -type f` (warm) | 139 | 187 | 0.74 |
| `layr tool find -type f` (warm) | 22 | 21 | 1.05 |
| edit 10 files | 5 | 4 | 1.25 |
| save 10 | 57 | 240 | 0.24 |
| append to 10,000 files | 586 | 337 | 1.74 |
| save 10,000 | 352 | 3,938 | 0.09 |
| line delete (10,010 changes) | 556 | 898 | 0.62 |

layr repository:

| Step | FUSE | Btrfs | FUSE / Btrfs |
| --- | ---: | ---: | ---: |
| line new | 36 | 24 | 1.50 |
| line delete (clean) | 23 | 13 | 1.77 |
| tool index ready | 87 | 77 | 1.13 |
| `git status` (cold) | 18 | 192 | 0.09 |
| `git status` (warm) | 9 | 12 | 0.75 |
| `rg -l TODO` through the mount (first) | 85 | 22 | 3.86 |
| `rg -l TODO` through the mount (warm) | 13 | 18 | 0.72 |
| `layr tool rg -l TODO` (warm) | 17 | 14 | 1.21 |
| `find -type f` (warm) | 25 | 15 | 1.67 |
| `layr tool find -type f` (warm) | 10 | 13 | 0.77 |
| edit 10 files | 4 | 4 | 1.00 |
| save 10 | 17 | 64 | 0.27 |
| create 10,000 files | 2,111 | 1,254 | 1.68 |
| save 10,000 | 55 | 1,960 | 0.03 |
| line delete (10,010 changes) | 58 | 63 | 0.92 |
| `cargo build` (full) | 65.1 s | 65.5 s | 0.99 |
| `cargo build` (touch `main.rs`) | 6.5 s | 6.2 s | 1.05 |
| `cargo build` (edit `views.rs`) | 295 | 230 | 1.28 |

Steps that differ by a few milliseconds (line new and delete, `find` on the small tree, edit
10 files) change from round to round by more than the difference.

### Where the view is still slower, and why

- **First read through the mount (`rg` first: 5.4 times).** Each file costs one READ round trip
  the first time, and the view's pages are a second copy of the blob's pages. The kernel shares
  the backing pages only with FUSE passthrough, which needs `CAP_SYS_ADMIN` in the server. Agents
  use `layr tool rg`, which reads the blobs directly.
- **Many small writes (1.7 times).** Each first write of a file is at least one round trip (a
  create costs three: lookup, create, write). With io_uring a round trip costs about 25 us on this
  machine; Btrfs needs no round trip.
- **`layr tool rg` (1.2 times).** Each query looks up the backing path of each file and checks
  that the hot blob exists.
- **Line new (about 7 ms).** `fusermount3` runs for each mount; 32 ring threads start.

### New root needs

- `fuse.enable_uring=1` (`/sys/module/fuse/parameters/enable_uring`, root, once per boot).
  Without it `--uring` falls back to `/dev/fuse` (the threads end; requests keep working).

## Results of version 1 (2026-11-15)

Raw data: [benchmarks/layrfs-fuse-20261115](../benchmarks/layrfs-fuse-20261115/README.md) (this
version) and [benchmarks/layrfs-fuse-20261114](../benchmarks/layrfs-fuse-20261114/README.md)
(the first version).

Machine: kukuka-wsl, WSL2, Linux 6.18, AMD Ryzen 9 5950X (32 threads), 19 GB RAM, git 2.53.
`/home` is btrfs with `compress=zstd:1`. Each step is the median of 3 rounds (each round runs
the three backends one after the other; "warm" steps are the median of 3 runs in a round). Load
average 1.5 to 10.9 during the rounds, from other users' `python`, `twin_fix` and `freecadcmd`
jobs (`load.txt`). The page cache was not dropped.

Backends:

- `fuse`: `layr fs view`. Store and upper are on `/home` (btrfs). git runs with
  `-c core.fsmonitor="layr fs view fsmonitor" -c core.untrackedCache=true`.
- `btrfs`: line = writable snapshot of a base subvolume. Save = read-only snapshot +
  `btrfs send --no-data -p` + `btrfs receive --dump` + the object write of the listed paths.
- `overlay`: OverlayFS on XFS (a loop file, reflinks). Save = a scan of the upper folder + the
  object write.

All saves write the same v2 objects with the same code (`layr-objects`): equal edits give equal
trees, `.git` aside (git's index stores inode numbers). On the native backends, `layr tool` uses
the user's own `layr fs serve`. Times in milliseconds unless marked.

### Before and after

The FUSE view before (2026-11-14, one run, load average 12 to 25, rg and find through the mount)
and after (median of 3), with Btrfs now as the reference. Linux kernel tree, except the builds
(layr repository).

| Step | FUSE before | FUSE after | Btrfs now |
| --- | ---: | ---: | ---: |
| `rg -l TODO` (agent: `layr tool rg`, warm) | 3,601 | 552 | 153 |
| `rg -l TODO` through the mount (warm) | 3,601 | 2,498 | 197 |
| `find . -type f` (agent: `layr tool find`, warm) | 880 | 18 | 20 |
| `find . -type f` through the mount (warm) | 880 | 128 | 160 |
| `git status` (warm) | 777 | 107 | 181 |
| `git status` (cold: first in a new line) | 70.7 s | 42.6 s | 16.0 s |
| line new | 33 | 28 | 15 |
| save 10 | 250 | 47 | 95 |
| save 10,000 | 3,789 | 545 | 4,000 |
| append to 10,000 files | 10.8 s | 6.5 s | 0.30 s |
| `cargo build` (full, layr) | 124.5 s, 64.6 s | 61.5 s | 61.0 s |
| `cargo build` (touch `main.rs`, layr) | 7.8 s, 7.3 s | 6.1 s | 5.7 s |

"Before" has two build values: the two rounds of 2026-11-14. The load was higher before, and the
Btrfs reference was slower too (warm `git status` 249 ms, save 10,000 5.6 s), so compare each
row with the Btrfs column.

### Linux kernel (97,294 files with `.git`, 2.0 GB)

| Step | FUSE | Btrfs | OverlayFS |
| --- | ---: | ---: | ---: |
| line new | 28 | 15 | 15 |
| line delete (clean) | 23 | 13 | 20 |
| index for `layr tool` ready | 1,610 | 938 | 758 |
| `git status` (cold) | 42.6 s | 16.0 s | 8.4 s |
| `git status` (warm) | 107 | 181 | 167 |
| `git status` (warm, FUSE without fsmonitor) | 123 | | |
| `rg -l TODO` (first, through the mount) | 3,451 | 212 | 235 |
| `rg -l TODO` (warm, through the mount) | 2,498 | 197 | 229 |
| `layr tool rg -l TODO` (warm) | 552 | 153 | 167 |
| `find . -type f` (warm, through the mount) | 128 | 160 | 155 |
| `layr tool find . -type f` (warm) | 18 | 20 | 20 |
| edit 10 files | 12 | 4 | 9 |
| save 10 | 47 | 95 | 34 |
| append to 10,000 files | 6,462 | 298 | 3,107 |
| save 10,000 | 545 | 4,000 | 323 |
| line delete (10,010 changes) | 28 | 885 | 1,716 |

### layr repository (1,860 files with `.git`, 15 MB)

| Step | FUSE | Btrfs | OverlayFS |
| --- | ---: | ---: | ---: |
| line new | 35 | 17 | 17 |
| line delete (clean) | 25 | 12 | 22 |
| `git status` (cold) | 296 | 159 | 190 |
| `git status` (warm) | 18 | 10 | 9 |
| `rg -l TODO` (warm, through the mount) | 34 | 12 | 12 |
| `layr tool rg -l TODO` (warm) | 20 | 9 | 11 |
| `find . -type f` (warm, through the mount) | 11 | 12 | 13 |
| `layr tool find . -type f` (warm) | 9 | 12 | 11 |
| save 10 | 14 | 57 | 17 |
| create 10,000 files | 6,655 | 1,101 | 1,370 |
| save 10,000 | 381 | 1,607 | 81 |
| line delete (10,010 changes) | 29 | 67 | 863 |
| `cargo build` (full) | 61.5 s | 61.0 s | 58.9 s |
| `cargo build` (touch `main.rs`) | 6.1 s | 5.7 s | 5.5 s |
| `cargo build` (edit `views.rs`) | 242 | 232 | 322 |

### Reading the numbers

- **Walks: as fast as native.** `layr tool find` answers from the index (18 ms on the kernel).
  A `find` through the mount (128 ms) is now faster than on Btrfs (160 ms): the kernel answers
  it from its caches, with no request to the server.
- **`git status` warm: faster than native (107 against 181 ms).** git asks the hook for the
  changed paths, and the kernel answers git's `lstat` calls from its caches. Without the hook it
  takes 123 ms.
- **Content search: 3.6 times Btrfs (552 against 153 ms).** `layr tool rg` reads the backing
  files and opens nothing through the mount (`check.sh` counts the opens). The rest of the
  difference is the lookup of each file's backing path, one at a time, under the server's lock.
  A plain `rg` through the mount pays an `open` round trip per file (2.5 s).
- **`git status` cold: 2.7 times Btrfs.** Every backend gives the files new inode numbers, so
  git hashes every file once. The view reads the 2 GB through the server, once per new line.
- **Line new, line delete, save: as fast or faster.** A save of 10,000 changes takes 545 ms
  against 4.0 s with Btrfs: the server knows the change list and needs no snapshot and no send
  stream. OverlayFS saves fast too (323 ms), but deletes slowly (1.7 s, it removes the upper
  folder).
- **Small writes: about 0.6 ms more per file.** A create or a first write costs several requests
  (lookup, create or open, flush, setattr, release) and a clone into `upper/`. 10,000 appends
  take 6.5 s against 0.3 s with Btrfs (OverlayFS: 3.1 s, it copies up too).
- **Builds: no measurable cost** with `target/` native: 61.5 against 61.0 s.

### Correctness checks (`bench/check.sh`, 43 checks, all pass)

1. The same edits in the view and in a native folder give the same tree id. The edits are
   writes, appends, `truncate`, `chmod`, removal of files and folders, renames of lower files and
   folders (also over an existing file), symbolic links, `touch -d`, and new empty folders.
2. After `kill -9` of the server between two groups of edits, a restart replays the journal and
   the save gives the same tree id.
3. `git add`, `git commit` and `git status` work in the view. `link()` gives `EPERM`; git falls
   back to a rename.
4. Tamper model: the agent cannot list the store or remove a state. Another user (`nobody`)
   cannot read the mount, also with warm caches, and cannot ask for a save.
5. `layr tool rg` (7 queries) and `layr tool find` (5 queries) give the output and exit code of
   the real tools on the mount. This is checked at the start, at once after writes, renames,
   removals, a `chmod`, a new folder, a save, a `kill -9` and restart, and a reset. A served
   search opens no file through the mount.
6. `git status --porcelain -uall` with the hook equals git status without it (a copy of the
   index, `core.fsmonitor` off). This is checked after the same steps. The server counts the
   hook's answers with a change list, so the test fails if git always gets `/`.
7. A write is visible at once through the mount and to `layr tool rg`, after a save too. A reset
   shows the head at once: the kernel's cached names and pages are invalidated.

Defects that these checks found in this version, now fixed:

- A full rebuild of the index lost the reader of backing files: searches went through the mount.
- The first query rebuilt the whole index: the walk did not record the time of `.git/info/exclude`.
- Another user could read cached folders: the kernel answered without the server's uid check.
- A dead mount was not found: `stat` answered from the attribute cache (now `statfs`).

## Root needs

| Root need today | With the FUSE view |
| --- | --- |
| `btrfs send` change list (`src/changes.rs`) | gone: the server's change marks |
| subvolume create, snapshot, delete (`src/sys.rs`) | gone: a state is objects, a line is a mount and an upper folder |
| OverlayFS mounts (`src/overlayfs.rs`) | gone: `fusermount3` mounts as the service user |
| `chown` of lines | gone: the view shows the agent as owner (`--owner`) |
| `TREE_SEARCH_V2` accounting (`layr du`) | gone for lines: the cost of a line is its upper folder and its new objects |
| root daemon that owns states and the database (docs/design.md section 2) | a service user (`layr`) is enough: the store is 0700 and the agent reaches it only through the socket and the mount |
| `setfsuid` run-as-caller (`src/privs.rs`) | stays as a problem: a non-root service cannot act as the caller. Commands named by a caller (git network access, merge drivers, checks) must run in the caller's own process, the CLI |
| privileged checks (sandboxes) | not changed by the view; unprivileged user namespaces still need `kernel.apparmor_restrict_unprivileged_userns=0` on Ubuntu |
| ext4 | no full tree copies: a line costs no copy, a first write copies one file (no reflink on ext4), and a save copies the changed files into `blobs/` |

Install-time steps that need root once (`bench/setup.sh`, log in `SYSTEM_CHANGES`):

- create the user `layr`;
- add `user_allow_other` to `/etc/fuse.conf`;
- install the binaries.

At run time, root appears only inside the setuid helper `fusermount3`, which the fuse3 package
installs.

FUSE passthrough (`FOPEN_PASSTHROUGH`, kernel 6.9 and later) would give native read speed. It
needs `CAP_SYS_ADMIN` in the server (`FUSE_DEV_IOC_BACKING_OPEN`), so it is not available to a
non-root service. The view does not use it.

## Verdict

Use the FUSE view for agent lines on Linux. It removes the root need of every line operation and
save: subvolumes, `btrfs send`, OverlayFS, `chown`. Then a service user can own the store.

| Area | Cost or gain against Btrfs |
| --- | --- |
| line new, line delete | 13 to 18 ms more per line new; a delete with many changes is faster |
| save of 10,000 changes | gain: 4 to 7 times faster |
| `find`, `layr tool find`, warm `git status` | as fast or faster |
| content search (`layr tool rg`) | 3.6 times slower on the kernel (0.55 s) |
| cold `git status` in a new line | 2.7 times slower |
| first writes of many small files | about 0.6 ms more per file |
| builds | no cost, with `target/` native |

## What remains

- Connect the view to `layr` itself:
  - `line new --view virtual` on Linux;
  - the state records and the database (the view writes object states only);
  - `status` with ignore rules;
  - more than one line per server;
  - the fsmonitor configuration of a new line.
- Move run-as-caller commands into the CLI.
- Content search: look up all backing paths in one pass under one lock, not one per file.
- Cold `git status`: the view could give git stable inode numbers and the saved times, so that
  the index of a state needs no new hashing.
- The cost per small write: fewer requests per create (no `setattr` after the writeback), and no
  second `open` of a new upper file.
- Hard links (`link` gives `EPERM`), extended attributes in writes (`ENOTSUP`), `RENAME_EXCHANGE`.
- A save takes the lock of the whole tree. Writes wait for it.
- Measure on a machine without other load, and with a cold page cache.

## Reproduce

```sh
cargo build --release --features layrfs-fuse          # layr with the view
(cd tools/layrfs-fuse && cargo build --release)       # the benchmark helpers
export B=$HOME/layrfs-bench LF=$PWD/tools/layrfs-fuse/target/release/layrfs-fuse LAYR=$PWD/target/release/layr
tools/layrfs-fuse/bench/setup.sh                       # once; uses sudo, logs SYSTEM_CHANGES
tools/layrfs-fuse/bench/check.sh
tools/layrfs-fuse/bench/run3.sh                        # bench.sh on both trees, 3 rounds
```

The source trees must be readable by the user `layr` (`/home/layrfs-src`, mode 0755).
