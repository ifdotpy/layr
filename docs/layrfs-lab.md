# LayrFS lab on Linux (2026-11-16)

This page is part of the [Layr documentation](../README.md#documentation).

A lab build of LayrFS on Linux (FUSE) tests what LayrFS can give layr beyond a Btrfs or
OverlayFS line. The code is in `src/layrfs/view` and `vendor/fuser`. The commands are lab
commands: they are not part of `layr` yet. Machine: kukuka-wsl (WSL2, kernel 6.18, 32 CPUs,
Btrfs with zstd). Trees: the Linux kernel (103,754 entries with `.git`) and the layr repository.
Scripts and raw results: [benchmarks/layrfs-lab-20261116](../benchmarks/layrfs-lab-20261116).

## Lab commands

```sh
layr fs view hub --sock <path> [--allow <uid>]...         # one process serves many lines
layr fs view ctl <hub sock> new <start options>...         # mount a line in the hub
layr fs view start ... --remote <objects>                  # blobs come from another store at first read
layr fs view ctl <sock> goto <state>                       # rest the view on another state, in place
layr fs view ctl <sock> rebase <onto> <name>               # three-way merge of tree objects
layr fs view ctl <sock> trace start|stop                   # the files that requests touched
```

## Results

Times are medians in ms. "LayrFS" is the FUSE view with `--no-open --uring`.

### One process for many lines (`hub`)

Twenty lines stay mounted; each new line is timed. Load average below 1.

| Step | LayrFS hub | LayrFS, process per line | Btrfs |
|---|---:|---:|---:|
| line new, Linux kernel | 21 | 30 | 19 |
| line new, layr | 12 | 20 | 15 |
| line delete, Linux kernel | 13 | 12 | 12 |
| line delete, layr | 10 | 9 | 10 |

- In the hub, a line is mounted in 7.9 ms. The rest is the client process and its socket.
- `fusermount3` now starts with `posix_spawn`, not `fork` (patch in `vendor/fuser`). A fork of a
  hub with 20 lines took up to 145 ms.
- A line delete answers once the mount is out of the folder tree. Before, it waited until the
  kernel freed every node: 210 ms after the index walk of the kernel tree, now 9 ms.
- Memory of 20 kernel lines: 2.0 GB (about 100 MB per line, mostly the index of `layr tool`).
  FUSE over io_uring adds about 13 MB per line (one thread and ring per CPU).

### Root

| Step | Btrfs without root | LayrFS |
|---|---|---|
| line new (snapshot) | yes | yes |
| read-only snapshot | yes | not needed |
| save (`btrfs send`) | no: CAP_SYS_ADMIN | yes |
| line delete (subvolume delete) | no: needs the mount option `user_subvol_rm_allowed` | yes |

Every LayrFS step ran as the service user `layr` or as the agent user, with no root.

### A saved state after every agent step

50 steps on the kernel tree, 20 changed files per step.

| Step | LayrFS | Btrfs |
|---|---:|---:|
| save as a layr state after each step | 15 | 82 |
| read-only snapshot only (no layr state) | | 15 |
| back to step 10 | 42 | 26 |
| forward to step 50 | 15 | |

- LayrFS goes back in place: the folder, its open files and the shell's current folder stay.
  Btrfs deletes the subvolume and makes a new one.
- `goto` visits only folders whose tree objects differ and keeps every other node, so the kernel
  keeps their names and pages. The first version dropped every node: 1984 ms.

### Rebase onto a new upstream state

Upstream: 3,000 changed files, 200 new files, 200 removed files. The agent line: 200 changed
files in other folders.

| Step | LayrFS | Btrfs |
|---|---:|---:|
| rebase 200 changes | 160 (saves the result as a state) | 1226 (no state) |
| rebase with 1 conflict | 21, names the path | |

- The merge works on tree object ids: a folder that one side did not change is taken whole.
  The merge itself took 1.3 ms; the save of the agent changes 17 ms; the update of the view and
  of the kernel caches 150 ms.
- The result has the same tree id as the same 200 edits made on the upstream state.
- Btrfs: read-only snapshot, `btrfs send --no-data` for the changed paths, `cmp` for conflicts,
  a snapshot of upstream, a copy of each changed file.

### Lazy load of blobs

The store of the line has trees and times but no blobs (128 MB instead of 2.8 GB). A read copies
the blob from the full store at first use. The full store was on the same disk: this measures
what is fetched, not the network.

| Step | Time | Fetched |
|---|---:|---|
| line new, kernel | 26 | |
| index of `layr tool` ready | | 387 files, 10.5 MB (ignore files, `.git/index`) |
| `git status` (cold), kernel | 2153 | 7,537 files, 364 MB (mostly `.git` packs) |
| `rg` in `kernel/sched` | 21 | 54 more files |
| `rg` in the whole kernel tree | 6859 | 96,627 files, 2.0 GB |
| `cargo build` (full), layr | 53,741 | 210 files, 5.1 MB |

### Files a build uses (`trace`)

`trace start` sets the timeout of names and attributes to zero and drops the kernel's caches of
the view, so every lookup, stat, listing and read reaches the server.

| Step | Time |
|---|---:|
| `trace start` (2,212 nodes) | 31 |
| no-op `cargo build` | 273 |
| no-op `cargo build`, traced | 501 |

A traced no-op build of layr touched 246 paths and read 11 files; a build after
`touch src/main.rs` read 131 files. Check of the prediction (append a comment, run
`cargo build`, count the compiled crates):

| Changed file | Files | Build compiled something |
|---|---:|---:|
| outside the traced set | 10 | 0 of 10 |
| inside the traced set | 10 | 9 of 10 (a comment in a `Cargo.toml` compiles nothing) |

So the traced set is a safe superset of the build's inputs: a change outside it cannot change
the build.

### FUSE passthrough

| `rg -l TODO`, kernel tree, new line | first | warm |
|---|---:|---:|
| LayrFS `--no-open --uring` | 1228 | 441 |
| LayrFS `--passthrough --uring` | 2917 | 2499 |
| LayrFS `--passthrough` | 3106 | 2630 |
| Btrfs | 2066 | 211 |

- Passthrough works (the server gets no reads), but every `open` and `release` still goes to
  the server: 195,770 opens for two runs. The no-open mode has no open requests at all.
- The kernel gives no passthrough with the writeback cache, so `--passthrough` turns it off.
- Result: passthrough is not worth `CAP_SYS_ADMIN` for agent tools.

### First read in a new line

| `rg -l TODO` first, kernel tree | line 1 | line 2 | line 3 |
|---|---:|---:|---:|
| Btrfs, new snapshot each | 1595 | 1874 | 1717 |
| LayrFS, new line each | 1191 | 1110 | 1165 |

A Btrfs snapshot has its own inodes, so each new line reads the files from disk again. LayrFS
reads the shared blob files, whose pages stay in memory for every line. In the earlier benchmark
the Btrfs `rg (first)` step was warm, because the cold `git status` before it had read every file.

## Fast lines in layr (Linux)

```sh
layr line new topic --fast         # a LayrFS mount from the hub of the service
layr line new topic --classic      # a Btrfs subvolume (the default)
layr config line.kind fast         # the project's default; it falls back to classic with a note
```

A fast line takes every command of a classic line, with the same rules (index, untracked files,
park, merge, undo). `tests/fast-parity.sh` runs 29 commands on a line of each kind and compares
their output: equal.

- The service (`layr daemon`, root) starts one hub per machine at `<socket folder>/layrfs-hub.sock`
  and asks it for each line. The control socket of a line is `<socket folder>/views/<id>.sock`;
  only the line's owner may use the mount and the socket.
- A fast line needs object states (`layr migrate`). Every Linux build has the feature
  `layrfs-fuse` (a default feature since 2026-11-17). `--fast` without objects fails with the
  reason; a `line.kind` of fast gives a classic line and a note. User guide: [fast-lines.md](fast-lines.md).
- How the folder paths work on a mount: every native snapshot of the line's folder
  (`storage::snapshot`) asks the view for its content as objects, writes the changed paths into
  the line's shadow subvolume (`meta/vlines/<id>/shadow`, first made as a snapshot of a shared
  checkout of the start state, `meta/shadows/<tree>-<times>`), and snapshots the shadow.
- Such a snapshot is registered with its objects (`object_tree::register`): a comparison with it
  is a comparison of objects, which skips folders with equal objects. A writable copy (the index,
  a work folder) keeps its base; its later read-only snapshot gets objects written for the
  changed paths only (`btrfs send --no-data` against the base). Only copies of a shadow are
  bases: a classic save that copies a state folder (`meta/stage`) gets its objects by the usual
  import, which writes deltas (tests/t200-objects.sh).
- A line whose server is gone (a crash of the service, a restart) is mounted again: by the
  service at its start (a child of the service loop waits for the hub, then mounts every fast
  line of the machine), and by a command that reads the line (`Repo::working_src`). A dead mount
  (ENOTCONN) is unmounted first. tests/fast-crash.sh: SIGKILL of the service and a lost mount;
  the line is back in 0.1 to 0.2 s with its unsaved changes. The rollback check of the service
  skips fast lines (their folder is an empty mount point while unmounted).
- Tests: `tests/fast-parity.sh` and `tests/views-layrfs.sh` (root daemon on an isolated store):
  pass.

Through `layr`, median of 3 (load average 5 to 8):

| Step | layr classic | layr fast | kernel classic | kernel fast |
|---|---:|---:|---:|---:|
| line new | 120 | 94 | 2,593 | 108 |
| first status | 208 | 95 | 19,869 | 140 |
| save 10 changed files | 602 | 126 | 91,022 | 304 |
| line delete | 431 | 89 | 46,839 | 97 |

The kernel classic numbers are from before the object bases (a later classic save took 41 s:
its scan of the working folder is still a full compare with the objects).

Earlier version (fast lines with the virtual-line rules, before parity), through `layr`, median of 5, layr repository, load average about 10:

| Step | Btrfs folder line | Btrfs virtual line | LayrFS line |
|---|---:|---:|---:|
| line new | 174 | 275 | 164 |
| save 10 changed files | 1139 | 1289 | 78 |
| line delete | 635 | 766 | 97 |

The Linux kernel tree (without `.git`, a project with object states, load average 4 to 19):

| Step | Btrfs folder line | Btrfs virtual line | LayrFS line |
|---|---:|---:|---:|
| line new | 2524 | 2547 | 96 |
| save 10 changed files | 60,030 | 60,383 | 688 |
| line delete | 53,721 | 54,003 | 661 |

The Btrfs saves of a project with object states write the objects of the saved snapshot, and a
delete saves the working state first; LayrFS writes only the changed paths.

`tests/views-parity.sh` on Btrfs fails at `line delete -D hidden` (unsaved changes need
`--discard`). The build before this work fails at the same step.

## Fast lines on macOS (FSKit)

`layr line new <name> --fast` on macOS makes a LayrFS (FSKit) line marked fast: the same mount
and upper folder as a virtual line, but every command takes the folder paths.

- Reads of the line's folder go to its shadow (`Repo::working_src`): an APFS folder, first a clone
  of the lower tree, brought up to the view at each read: entries of the upper folder that
  changed since the last time (size, times, mode) are copied in with their times, removed paths
  go, and paths that left the upper folder come back from the lower tree. APFS clones, file ids
  and layr attributes work on it; the mount has none of them. Writes go to the mount.
- The extension's sandbox puts a quarantine mark on its upper files, and the mount refuses to set
  one: a copy into the mount skips it, and the shadow drops it.
- `line move` mounts the line again at the new folder; an unmount of a new mount is tried again
  for two seconds (the system looks at a new volume).
- `tests/fast-parity.sh` runs on macOS too (no daemon): 53 commands, the same output as a classic
  line. `tests/views-parity.sh` (virtual lines) still passes.

## Cleanup of fast line copies

`layr gc` and the service's hourly pass remove read-only shadow snapshots but the newest (after
ten minutes), shared start-state checkouts that no new line used for a day, and the shadows of
deleted lines (`tests/fast-sweep.sh`). A gc keeps the state under each fast line's changes, and
a snapshot of a view never gives its nodes blobs that no state names: before, a gc removed them
and a changed file read EIO.

## Fast lines inside `layr daemon` (Linux)

- `layr daemon` forks its service loop (the zygote: single threaded, one fork per command, as
  before) before any thread starts. The first process stays as the FUSE server of every fast
  line (`layrfs::view::host`, the hub socket `layrfs-hub.sock` next to the service socket). There
  is no separate hub process, and the two parts are always one version.
- A new `layr daemon` takes the lines of the running one (`takeover`): the old service loop stops
  (it waits for its requests), then each line moves (`handoff`): its readers stop between two
  requests (a SIGUSR2 interrupts their blocking read), the view's node table goes to the new
  process as JSON (the kernel keeps its node ids, so the numbers stay), and the /dev/fuse
  descriptor goes with SCM_RIGHTS. The new process serves the same connection without an INIT
  (`fuser::Session::from_fd_ready`). The mount never goes away.
- `tests/fast-upgrade.sh`: writes, reads, creates, listings and `rg` in a fast line while a new
  daemon starts; an open descriptor and the last write survive; status, save and delete work
  after. Three runs: pass, no error in about 1,100 loops each. A line moves in 10 ms; the new
  service answers 115 to 243 ms after its start.
- Cost: io_uring cannot hand over (a request that the kernel puts in a ring entry of the old
  process just before the ring closes gets no reply), so fast lines read /dev/fuse. Kernel tree,
  three rounds, load average about 20:

| Step | io_uring | /dev/fuse (pausable) | /dev/fuse, spin 50 µs |
|---|---:|---:|---:|
| first `rg` | 1.1 to 3.9 s | 2.8 to 3.2 s | 2.4 to 2.5 s |
| edit 10,000 files | 1.0 to 1.2 s | 1.9 to 2.0 s | 1.8 to 2.2 s |
| `find` | 0.23 to 0.27 s | 0.72 to 0.79 s | 0.59 to 0.65 s |

Decision (2026-11-16): fast lines use /dev/fuse, without io_uring, so that an upgrade of the
service never breaks a program in a line. io_uring comes back when the kernel can stop new
requests to a process's ring entries; layr would then check for it at INIT and use it when
present.

## Correctness

`tools/layrfs-fuse/bench/check.sh` with `--no-open --uring`: 43 passed, 0 failed.

## What this means for layr

1. Connect LayrFS to `layr` on Linux with one server per user (the hub). Line new and delete are
   as fast as Btrfs, and no step needs root.
2. Saved states per agent step, `goto` and rebase are cheap in LayrFS and slow or missing in
   Btrfs. They fit undo per tool call and automatic rebase of agent lines.
3. Lazy blobs let a machine open a line without the whole tree. Builds read a small part of it.
4. The trace gives exact build inputs: a base for `retest` and for build caches.
5. Do not use passthrough: it needs CAP_SYS_ADMIN and is slower for many small files.

## Open items

- The index of `layr tool` costs about 100 MB per kernel line, and its walk of the mount fills
  the kernel's caches. An index from memory, with the ignore rules applied in the server, would
  remove both.
- `goto` refuses a view with unsaved changes. A rebase with conflicts leaves the view on the
  saved agent changes.
- The hub takes the mount options from its client. A real service must check the paths.
- Lazy load over a network: not measured.

## Caches and speed of fast lines (2026-11-16)

### Caches

The folder of a fast line is a LayrFS mount: a nested subvolume (btrfs) or layer (APFS) cannot
live in it. The cache folders of a fast line live in `lines/.caches/<line id>/<folder>`, and the
line shows each one with a link at the folder's path (`src/fast_cache.rs`). LayrFS leaves links
into a cache root out of the line's content. `save --overlays`, `line new`, `merge`, `rm -rf
target` and `line delete` work as in a classic line. Test: `tests/fast-parity-more.sh` (40
commands: store remotes, caches, checks, queue, retest), on Linux and macOS.

`line delete` of a fast line with unsaved changes now needs `--discard`, as a classic line does.

### macOS speed (50,000 files, 195 MB, a loaded machine)

| step | fast before | fast now | classic now |
|---|---:|---:|---:|
| line new (start tree kept) | 8 s | 0.13 s | 7.9 s |
| first status | 140 s | 1.9 s | 13.7 s |
| status after an edit | 256 s | 0.25 s | 3.2 s |
| save | 478 s | 10 to 13 s | 6.6 s |

Fixes:
- A compare with an object head read every file out of the object store to compare its
  attributes (`stored_same`, `lmeta`, `same_entry`). It reads the tree entry now. Classic lines
  gain from this too.
- The shadow of a fast line knows where it can differ from its start state: the upper entries and
  removed paths. A compare with an object tree (or thin layers over one) reads those paths and the
  object diff of the start state to the head, with no walk of the whole tree (`views::Known`).
- The overlap compare of `status` read the mount through FSKit; it reads the shadow now.
- The native tree of a fast line's start stays a day after its last use (`fast-seeds`).
- A background trash emptier no longer keeps the caller's folder (a mount) busy.

Open: `save` still copies the whole shadow for its scan (about 5 s here) and scans it twice.

## Git in fast lines (2026-11-16)

A fast line has a `.git` folder, and git commands move the line (the rules:
[commands](commands.md#git-in-a-fast-line)). Before this work a fast line had no `.git` folder:
`git status` there failed with "not a git repository".

### How it works

- The `.git` folder is a native folder in the cache root of the line
  (`lines/.caches/<line id>/.git`). The mount shows it as a link, which LayrFS leaves out of the
  content of the line (`fast_cache`). So git reads and writes its own files with no FUSE request,
  hard links and `mmap` of packs work, and the server needs no change.
- The objects come from the git store of the project (`objects/info/alternates`), and the index
  is the kept index of the start state. Git compares only the size and the whole second of the
  time of a file (`core.checkStat=minimal`), and the view shows the saved times, so a new line
  hashes no file.
- `core.fsmonitor` is `layr fs view fsmonitor`, with `core.untrackedCache`: git asks the view for
  the changed paths.
- Git moves `HEAD`: a hook starts `layr git adopt --hook` in the background, and each layr
  command of the line owner checks first. The new commits go to the git store as a pack through
  a pipe (the git that reads the folder runs as the line owner). Each becomes a state
  (`plan_import_over`: only the commits that the old head does not reach are walked, not the
  whole history). The state gets three things at once, each from its first parent and the changed
  paths: its objects, its snapshot, and its git index. Then the head moves with the files kept
  (a mixed reset).
- layr moves the head: `git update-ref HEAD <commit> <agreed commit>` as the line owner, then the
  index of the state.

### Measurements

Machine and tree as above (kukuka-wsl; the Linux kernel, 97,294 files, 2.0 GB, here with a git
repository of one commit). Medians in ms of 5 rounds (2 for the classic line); load average 7 to
14. "Plain" is the same tree in a Btrfs folder with a git repository of its own, without layr.
Scripts and raw results: [benchmarks/git-view-20261116](../benchmarks/git-view-20261116).

| Step | plain git | classic line | fast line |
|---|---:|---:|---:|
| line new | | 15,776 | 786 |
| `git status` (first in a new line) | | 222 | 386 |
| `git status` (warm) | 272 | 188 | 49 |
| `git status` (warm, fsmonitor off) | | | 189 |
| `git status` after 10 edits | 180 | 274 | 104 |
| `git diff --stat` | 48 | 50 | 43 |
| `git commit -a` (10 files) | 114 | refused (read only) | 131 |
| the head follows (`layr git adopt`) | | | 1,802 |
| `layr status` after the git commit | | | 178 |
| `git status` after the git commit | | | 119 |
| `git reset --hard HEAD~1` | 357 | refused (read only) | 236 |
| `layr status` (first) | | 24,892 | 229 |
| `layr save -a` (10 files) | | 58,336 | 1,538 |
| `git status` after the layr save | | 186 | 227 |
| `layr reset --hard head~1` | | 64,426 | 36,267 |
| line delete | | 53,510 | 1,020 |

- **`git status` (warm) is 3.8 times faster than in a classic line** and 5.5 times faster than
  plain git on Btrfs (49 against 188 and 272 ms). Without the fsmonitor hook the fast line takes
  189 ms, as the classic line does.
- **`git commit` costs what plain git costs** (131 against 114 ms). The head of the line is there
  1.8 s later; a layr command that comes sooner waits for it.
- **The first `git status` of a new line is slower** (386 against 222 ms): the first question to
  the hook has no token, so git reads every folder once and writes the untracked cache.
- **`layr reset --hard` to an earlier state is slow in both kinds** (36 and 64 s): it writes every
  file of the state from the objects (`objstore::build_state`). This is not a git cost. The same
  change as for a git commit (a copy of a near snapshot with the changed paths,
  `objstore::build_state_near`) would remove it; it is not connected to `reset` yet.

What the first build cost on this tree, and what removed it (`results-first.jsonl`,
`results-sync-hook.jsonl`; `scripts/trace.sh`):

| Step | first build | now | Cause |
|---|---:|---:|---|
| `layr save -a` in a line with `.git` | 39,563 (classic 103,443) | 1,538 | the update of the git index of a state wrote the whole tree of the state, twice, for 10 changed paths; it now gets a folder with those paths (`work_tree`) |
| the head follows a git commit | 16,620 | 1,802 | a mixed reset built the tree of its target; the state of the commit had no snapshot and no index |
| `layr save` after a git commit or `git reset` | 15,467 | 1,538 | the save needs the snapshot of the head; the state of HEAD now gets one from a near snapshot |
| `git commit -a` | 2,458 (hook waits) | 131 | the hook runs in the background |

The classic line gains from the first row too: its save went from 103 s to 58 s (the rest is the
cost of a classic line in a project with object states, measured before).

### Correctness

- `tests/fast-git.sh` (57 checks, 6 of 6 runs pass): a commit, a commit of one path, amend, reset,
  another branch, undo of layr, rebase, merge, two commits without hooks, a tree change (removed
  and moved files, a folder that goes, a link, a mode) whose state equals the folder, a push with
  git's commit ids, a fetch, a new line from a state that git made, fsmonitor against a full scan,
  a protected line, line delete.
- `tests/fast-parity.sh` (53 commands) and `tests/fast-parity-more.sh` (40 commands): pass. The
  first now gives the project its git store before the lines, so both lines have a `.git` folder.
- `tests/views-layrfs.sh`: pass.

### Limits and open items

- macOS (FSKit): the code is the same (no fsmonitor there), but it did not run: the LayrFS
  extension did not mount in the session of this work.
- A branch of git is a name in the folder of one line. Other lines and the cloud do not see it.
- `git stash`, `git worktree`, submodules and `git remote add` are git's own in the folder.
- A layr command and a git command that move the head at the same time: the branch of git stays
  where git put it, the layr command warns, and the head of the line follows git. The state of
  the layr command stays in the history of the line (`layr undo`).
- Each git checkout of another branch is a move of the head of the line, with a record.
- The server still cannot do a `git checkout` in place (`goto`): git writes each changed file
  through the mount.
- Found on the way, not changed: with both lines of `tests/fast-parity.sh` made before the first
  command, the merge in the fast line reported one more conflict (`f3`, the moved file) in 2 of 5
  runs. The order of the test hides it.

## Git index and `.git` inside LayrFS (2026-11-17)

Data and scripts: `benchmarks/git-index-20261117/`. Kernel tree, kukuka-wsl, medians in ms.

**Split index.** The `.git` folder of a fast line has `core.splitIndex`. After a layr move, git
makes the index of HEAD from its own index (`git reset --no-refresh` as the line owner) instead
of a copy of the whole index that layr keeps. Old (`0d21b45`) against new, 7 alternating rounds:

| Step | old | new |
|---|---:|---:|
| `git status` after 10 edits | 114 | 77 |
| `git commit -a` (10 files) | 152 | 112 |
| `git status` after `git commit` | 122 | 74 |
| `git reset --hard HEAD~1` | 237 | 178 |
| `git status` after `layr save` | 248 | 132 |

**`.git` inside the mount** (the way to see `git worktree add` and HEAD moves without a hook):
git is slower there because two kinds of writes go through FUSE: the whole index (10 MB) at each
status and commit, and about 30 loose tree objects per commit. With a split index and an
`objects` folder outside the mount, git inside is as fast as git outside today (`git commit`
110 against 143, `git status` 95 against 88). `core.fsync=none` changes nothing.

**`git worktree add`.** 12.8 to 13.7 s in plain git; 1.4 s when a `reference-transaction` hook
makes a fast line and mounts it at the path before git checks out. Git then writes no file, as
long as the index of the new worktree is ready. Open: layr does not follow HEAD of such a worktree,
`git worktree remove` wants a `.git` file (the view shows a link), a path next to the line is in
the root-owned `lines/` folder, and layr does not know the line by the new path.

## Six items measured (2026-11-17)

Kernel tree, kukuka-wsl, load 6 to 13. Scripts: `benchmarks/git-index-20261117/` (`adopt.sh`, `rh.sh`, `ck.sh`, `ck3.sh`) and `scripts/`.

- **The head follows `git commit`** (`adopt.sh`): `layr git adopt` takes 1.7 s. Of it: the
  import of the commit 0.35 s, two walks for regenerable folders 0.31 s, the save of the working
  state before the mixed reset 0.14 s, the free step after the command 0.25 s (it opens every
  project of the store). Not changed yet.
- **`layr line delete` after a restart of the service** failed with EBUSY: the delete mounts the
  view again, and the view walks its mount for the index of `layr tool` for about 0.5 s; an
  unmount during the walk fails. Fixed: a stop waits for the walk (at most 10 s), and a delete
  without `--discard` mounts the line before its check for unsaved changes. After a restart
  without a handoff, the folder of a fast line stays empty until a layr command mounts it.
- **`layr reset --hard`** in a fast line: 81 s (first) and 39 s (`head~1`), 22 s forward. Two
  causes: the target state was written in full and then compared with the folder by content, and
  the bridge built the kept git index of the state (a hash of every file). Fixed: the target is
  built from the folder of the head with the paths that differ (`build_state_near`), a fast line
  compares object ids, and the bridge builds the kept index only when git has none. Now 1.5 s
  back and 1.1 s forward.
- **A switch between two commits that differ in 1,260 files** (`ck.sh`, `ck3.sh`): plain git
  `checkout` 420 ms; git in a fast line 1.8 s (git writes each file through FUSE); `layr reset
  --hard` 4.6 to 7.6 s; the view's `goto` 75 to 700 ms (no file written, mostly the invalidation
  of kernel caches). `goto` refuses a view with changes, and the `.git` link of a fast line counts
  as one, so a `git checkout` in place needs `goto` to keep cache links first.
- **`git worktree add`**: see above (1.4 s against 12.8 to 13.7 s).

## The git commands of T3 Code in a fast line (2026-11-17)

`scripts/t3.sh` runs the commands of T3 Code: a checkpoint (`read-tree` and `add -A` into a
temporary index, `write-tree`, `commit-tree`, `update-ref refs/t3code/...`), a diff against it,
a restore (`restore -s <ref> -- .`, `clean -fd`), `switch -c`, `commit`, `remote add`,
`push -u`, `fetch`, `pull --ff-only`, `worktree add`. All work in a fast line; refs under
`refs/t3code/` stay refs of the folder (the line follows only `HEAD` and `refs/heads/*`).

Two faults found and fixed: a cache link (`target`) was untracked for git (a checkpoint took it,
and `clean -fd` removed it), and the fsmonitor hook printed errors in a worktree that is no view.

Kernel tree (`scripts/t3k.sh`, load 54, so only the order of size counts):

| Step | plain git | fast line |
|---|---:|---:|
| checkpoint as T3 makes it (`read-tree HEAD`, `add -A`) | 7,952 to 25,866 | 72,241 to 154,775 |
| checkpoint with a kept temporary index | 304 to 326 | 1,903 to 2,519 |
| restore (`restore -s`, `clean -fd`) | 606 to 653 | 1,386 to 7,870 |
| `git diff --numstat HEAD <checkpoint>` | 19 to 65 | 141 to 278 |

`read-tree HEAD` gives an index without stat data, so `add -A` reads and hashes every file;
through FUSE that reads the whole tree.

### Why a T3 checkpoint is slow in a fast line (2026-11-17)

`read-tree HEAD` into a new index gives entries without stat data (size 0), and git then reads
and hashes every file, one after the other. Through the mount each file costs a read request and
a `getattr` (a read request drops the cached access time, so the next lstat asks the server).
Single process `cat` of 30,000 kernel files: 3.6 to 4.5 s cold, 0.6 to 0.9 s from the page cache.
Tried, without a gain on kukuka-wsl (load 35 to 69 from other work, 0.2 GB free memory):

- `--passthrough` (needs open requests: slower), `--spin 50|200`, `--threads 16` (`--passthrough` and `--spin` are removed since 2026-11-17);
- a warm-up with 8 readers through the mount after a full fsmonitor scan (1.6 GB in 3 to 7 s):
  the pages were evicted before git came, and the reads dropped the attributes;
- a warm-up with FUSE_NOTIFY_STORE (no read request, attributes stay): the prototype stored no
  file (the inode numbers of a walk of the mount were not the nodes of the view). Open.

With the pages in the cache, `add -A` into a new index takes 8 to 9 s, as in a plain folder.

**Fixed (same day).** Two causes made the first checkpoint of a new line slow:

1. Git wrote a loose copy of every object (96,705 for the kernel, 1.6 GB, 20 s user and 25 s
   system time). Before git skips the write of an object it has, it touches the file that holds
   it (freshen); the packs of the git store belong to root, so the touch failed. A fast line's
   `.git` now gets reflink clones of the store's packs in `objects/pack`, owned by the line owner
   (`clone_store_packs`).
2. Every file cost a read request through the mount. After a full fsmonitor scan, the view watches
   its read requests for 5 s; when git reads 200 files or more (it hashes them), the view stores
   the content of every unchanged file into the kernel's page cache (FUSE_NOTIFY_STORE, 4
   threads, at most 2 GB and once in 10 minutes per view). A store makes no read request, so the
   kernel keeps the attributes too. A plain `git status` with a full scan reads no file and
   starts nothing. A file written while the store runs is invalidated after its store.

Kernel tree, a new fast line each round (`scripts/tm3.sh`, load 22 to 24):

| | first checkpoint | its `add -A` | second checkpoint | restore |
|---|---:|---:|---:|---:|
| before | 57,000 to 193,000 | | | |
| clones of the packs | 17,750 to 21,233 | 17,050 to 20,530 | 5,893 to 6,171 | 599 to 628 |
| clones and the store | 8,138 to 13,613 | 7,550 to 12,860 | 5,857 to 7,422 | 561 to 963 |

The store took 1.6 GB in 3.1 to 3.9 s. Plain git: 8 to 26 s for the same checkpoint.
`tests/fast-git-t3.sh` checks the commands of T3 Code and the content of every file after a store,
also of files written while it runs.

## `git checkout` in a fast line, and `layr undo` after it (2026-11-17)

- `git checkout` writes each file through the mount: about 8 requests per file (lookup, unlink,
  create, two writes, three getattr). 787 to 1,260 files: 2.4 to 3.1 s with one writer, 1.4 to
  2.0 s with `checkout.workers = 0` (now set); plain git 0.5 to 1.1 s. A hook cannot replace the
  write: git writes the files before it moves `HEAD`.
- `layr undo` after a followed checkout moved the branch that `HEAD` was on to the old commit (it
  rewrote that branch) and left the files of the other branch. Now git checks the branch it came
  from out again (`checkout_source`, from git's reflog of `HEAD`).
- That undo took 194 to 220 s on the kernel tree: two states that only the object store held were
  written in full, then compared by content (`btrfs send` 81 s, file reads 126 s). Now a fast line
  reverts its working folder from the objects of the two states (`revert_working`), and
  `check_state` builds a missing state from the folder of a line head with the paths that differ
  (`build_near_a_head`) before it writes every file. Now 12.7 s, of which the two builds 6.8 s
  (`scripts/ck5.sh`, load 39).
## Production readiness (2026-11-17)

- Every Linux build has LayrFS: `layrfs-fuse` is a default feature; the static musl release
  builds (a patch of `vendor/fuser` keeps its reader list `Send` on musl).
- CI: `tests/t262-fast-lines.sh` runs the fast-line tests (parity, caches, sweep, view contract,
  upgrade, crash) as a user against a daemon of the test store on Linux, and parity plus remount
  on macOS when the LayrFS extension is installed.
- macOS `save` of a fast line: 9 to 16 s before, 1.0 to 1.2 s now (50,000 files). The list of
  cache folders scanned the mount through FSKit; it scans the shadow now, plus the cache links of
  the upper folder (the shadow leaves them out).
- macOS: a fast line whose mount is gone is mounted again by a command (tests/fast-remount-macos.sh)
  and by the user service at its start. `line new --fast` checks that the extension is installed.
- `tools/layrfs/release.sh`: Developer ID signature, notarization and a stapled zip of
  LayrFSHost.app.
- The service loop (the child of the FUSE server process) gets SIGTERM when its parent goes: a
  kill of the daemon's pid stops the whole service, as before the fork (tests/t111-daemon-plist.sh).
- Caches bound to their build location (main, 2026-11-17): a fast line's cache tree carries the
  provenance; the line keeps the link. A new fast line relocates the caches it takes, or starts
  them cold; a plain folder that a tool makes at a cache path is refused as on a classic line.

