# Overlays

Status: design, not built. The numbers come from experiments on 2026-10-11 (section 5).

An overlay is a file tree with an owner, readers and its own history. A line shows several
overlays as one folder. With overlays, a project keeps files in history that not every reader
of the project may see: local settings such as `.env`, keys, personal notes, private data.
Today such files are either ignored (no history) or in history for every reader.

Short result:

- A commit can make the new state of each changed layer from its previous state plus its part
  of the change list. This took 10 to 13 ms per layer, for overlays of 10 to 10,000 files.
- Making the states from the full tree instead (snapshot and delete, empty subvolume and
  reflinks) grows with the overlay: about 27 ms for 10 files, 107 ms for 1,000 and 301 ms for
  10,000.
- Assembling a line from the shared state and the overlay files: 20, 48 and 144 ms.
- Sorting a change list by owner is cheap: under 1 ms for a commit of 54 files.
- A folder overlay as a nested subvolume costs 7 to 8 ms at any size. But `rename()` across
  its edge fails with EXDEV, `git mv` fails, and git can delete the subvolume.

## 1. Model

- An **overlay** is its own file tree. It has an owner, readers and its own history of states.
- The **shared set** is also an overlay. Its readers are everyone who reads the project. It is
  the only overlay that `layr push` sends to the git remote.
- A **line is a stack**: the shared set at the bottom, overlays above it. For the same path the
  upper overlay wins, as in overlayfs. The same path (for example `.env`) can be in many
  overlays.
- **New files go to the shared set.** A file goes to an overlay by an explicit command
  (`layr overlay move`) or by a routing pattern of the overlay. The patterns are in the
  overlay's metadata in the project database, not in a file of the tree, so a change of the
  tree cannot change where a file goes.
- **Overlays have their own lines.** In a stack, each overlay has one of three modes:

  | Mode      | The overlay line of a line                              |
  | --------- | ------------------------------------------------------- |
  | follow    | branches with the line and merges with it               |
  | shared    | one overlay line for all lines of the user              |
  | ephemeral | branches with the line, and is deleted when it merges   |

  The owner of the overlay sets the default mode. A line can override it.
- **A merge is atomic across the follow overlays.** The user who merges must be a reader of
  all of them. The checks run on the full result, with all layers in place.
- **Encryption.** In backups and in remote vaults, an overlay can be stored encrypted for its
  readers with age.

In this file a **layer** is the shared set or an overlay of a stack.

## 2. Commands

| Command                                         | Effect                                                         |
| ----------------------------------------------- | -------------------------------------------------------------- |
| `layr overlay create <name> --readers <principals>` | Makes an overlay. The caller is its owner. Principals are as in `layr access`: a user, `@group`, `*`. |
| `layr overlay route <name> <patterns>`          | New files whose path matches a pattern go to the overlay.      |
| `layr overlay move <name> <paths>`              | Moves files of the line to the overlay. The next commit removes them from the layer that had them. |
| `layr overlay use <names...>`                   | Sets the stack of the current line, from the bottom to the top, above the shared set. |

The other commands keep their meaning on the line as a whole: `status`, `diff`, `add`,
`commit`, `log`, `show`, `merge`. Section 7 lists what is not decided yet, for example the
place where a mode is set.

## 3. How it maps to btrfs

### 3.1 The line stays one subvolume

There is no overlayfs. A line is one writable subvolume that holds the files of all its
layers, so every program sees a normal folder. Each layer keeps its own states (read-only
subvolumes). The head of a line becomes a set: one state of the shared set and one state per
overlay of the stack.

Next to the head, the line keeps a read-only snapshot of its full tree: the **base**. The line
is a writable snapshot of the base, so `status` and `diff` still compare two snapshots of one
tree with `btrfs send --no-data`, in O(changes), as today.

### 3.2 Commit

1. The change list of the commit comes from `btrfs send --no-data`, as today.
2. Each changed file gets an owner: the highest overlay of the stack whose head state has the
   path; else the highest overlay with a routing pattern that matches the path; else the shared
   set. Folders belong to no layer: a layer has a folder when it has a file in it.
3. Each layer with changes gets a new state.
4. One record names the new states and the new base.

There are two ways to make the new state of a layer (step 3):

- **Split** (from the full tree). The shared state is a writable snapshot of the line with the
  overlay files deleted, then made read-only. An overlay state is an empty subvolume plus
  reflink copies of the overlay files, then made read-only. The cost grows with the number of
  overlay files.
- **Apply** (from the previous state). A writable snapshot of the layer's previous state, plus
  the layer's part of the change list (reflink copies, deletes, moves), then made read-only.
  The cost is one snapshot per changed layer plus the changed files.

At 10,000 overlay files, split takes 125 ms for the shared state and 176 ms for the overlay
state; apply takes 10 to 13 ms per layer at every size (section 5). So a commit uses apply.
Split is for the first state of an overlay (`layr overlay move` of many files) and for a repair.

### 3.3 Assembling a line

A new line from a line with the same stack (follow mode, same readers) is a snapshot of the
source line's base: O(1), as `layr branch` is today.

A line needs assembly when its stack differs from the source: a line of a user who reads other
overlays, or a line after `layr overlay use`. Assembly is a writable snapshot of the shared
state, then reflink copies of the files of each overlay, from the bottom to the top, so that
the upper file wins. A read-only snapshot of the result becomes the base. The cost grows with
the number of overlay files: 20 ms for 10 files, 48 ms for 1,000 and 144 ms for 10,000.

### 3.4 Folder overlays

An overlay that holds one whole folder can be a nested subvolume at that folder, as a cache is.
A snapshot of the line leaves a nested subvolume out: the snapshot has an empty folder there.
So:

- The shared state is a read-only snapshot of the line. Nothing is deleted.
- The overlay state is a read-only snapshot of the nested subvolume.
- Assembly is a snapshot of the shared state, `rmdir` of the empty folder, and a snapshot of
  the overlay state at that path.
- Each layer has its own `btrfs send --no-data` change list. No file needs an owner lookup.

This took 7 to 8 ms for split or assembly, for a folder of 185 files and for one of 8,117
files. The cost is at the edge of the folder: section 6.

### 3.5 Push, backups and replication

- `layr push` exports only the states of the shared set. A commit that changed only overlays
  makes no shared state, so git gets no commit for it.
- `layr backup` already writes one btrfs stream per state. The stream of an overlay state can
  be encrypted with age for the overlay's readers. The manifest then holds the SHA-256 of the
  encrypted bytes, and the signature covers them as today.
- `layr sync` sends states to trusted machines. Which overlays go to which machine is open
  (section 7).

## 4. Who sees what

- A user sees the files of an overlay only as a reader of it. `layr overlay use` needs that.
- `show`, `diff`, `grep`, `blame`, `log -p`, `try` and `export` of an overlay state need the
  read right of the overlay, in addition to the role in the project.
- The base and the automatic states of a line hold the files of all its layers. Section 7,
  question 5.

## 5. Measurements

### 5.1 Method

- Machine: an OrbStack Linux VM on an Apple M5 Max, 18 vCPU, 15 GiB memory. Linux 7.0.14
  (OrbStack build), btrfs-progs 6.17.1, GNU coreutils 9.7, Python 3.14.4, git 2.53.0.
- File system: btrfs on an 8 GiB loop file, made with the mkfs.btrfs defaults (16 KiB nodes,
  no-holes, free-space-tree) and mounted with `ssd,discard=async,space_cache=v2`. The loop
  file is on the btrfs disk of the VM.
- Tree: a copy of the working folder of a real application (a desktop program, a Python
  server, a web client, built packages), without version control folders and installed
  packages: 10,824 files in 1,032 folders, 609 MB.
- Overlays: 10, 1,000 and 10,000 files, picked at random over all files with a fixed seed.
  10,000 files are 92% of the tree.
- Every timed step ends with `btrfs filesystem sync`, so it includes the btrfs transaction
  commit. The file system is synced before each run. The times include the start of the
  `btrfs`, `xargs`, `cp` and `rm` processes (0.3 to 0.5 ms each); layr calls the ioctls
  directly. Reflink copies use `cp -a --reflink=always --parents`.
- 5 runs of each step. The tables give the median and the range (minimum to maximum) in
  milliseconds. In a second full run, the medians of the totals were within 7% for steps above
  40 ms, and within 40% for the short steps.
- The page cache was warm, and the file system was small and quiet. On a large file system
  under write load, one snapshot takes 15 to 600 ms (README, Performance).

The scripts are in `docs/overlays-bench/` (appendix).

### 5.2 Reference

| Step                                                     | Median | Range        |
| -------------------------------------------------------- | -----: | ------------ |
| one read-only snapshot of the line (a commit today)      | 6.0    | 4.3 to 7.5   |
| four read-only snapshots (one per layer of a stack of 4) | 18.8   | 17.2 to 25.0 |

### 5.3 Commit, split method (from the full tree)

| Step                                         | 10 files            | 1,000 files          | 10,000 files           |
| -------------------------------------------- | ------------------- | -------------------- | ---------------------- |
| shared: writable snapshot of the line        | 4.7                 | 12.7                 | 4.5                    |
| shared: delete the overlay files             | 1.3                 | 21.9                 | 78.5                   |
| shared: make read-only, sync                 | 13.4                | 37.9                 | 39.1                   |
| **shared state, total**                      | **19.6** (17.4 to 21.2) | **69.4** (63.5 to 116.0) | **124.7** (106.8 to 128.6) |
| overlay: create an empty subvolume           | 0.4                 | 0.5                  | 0.5                    |
| overlay: reflink copies of its files         | 1.2                 | 21.4                 | 139.0                  |
| overlay: make read-only, sync                | 6.1                 | 18.0                 | 27.2                   |
| **overlay state, total**                     | **7.8** (6.3 to 8.8) | **37.1** (34.4 to 41.9) | **176.0** (166.6 to 221.3) |

Per file at 10,000 files: about 8 µs to delete and 14 µs for a reflink copy.

### 5.4 Commit, apply method (from the previous state)

A writable snapshot of the overlay's previous state, reflink copies of 10 changed files,
read-only, sync.

| Overlay size | 10 files            | 1,000 files         | 10,000 files       |
| ------------ | ------------------- | ------------------- | ------------------ |
| total        | 12.5 (10.5 to 13.8) | 10.7 (8.3 to 15.8)  | 9.5 (7.8 to 10.6)  |

### 5.5 Assembling a line

| Step                                          | 10 files            | 1,000 files         | 10,000 files           |
| --------------------------------------------- | ------------------- | ------------------- | ---------------------- |
| writable snapshot of the shared state         | 4.9                 | 3.8                 | 3.4                    |
| reflink copies of the overlay files, sync     | 13.5                | 41.9                | 140.9                  |
| **total**                                     | **19.5** (17.1 to 21.7) | **48.0** (40.8 to 54.5) | **144.3** (134.0 to 145.9) |

A read-only snapshot of the result as the base adds one snapshot (5.2).

### 5.6 Folder overlay as a nested subvolume

| Step                                                                    | 185 files          | 8,117 files        |
| ----------------------------------------------------------------------- | ------------------ | ------------------ |
| split: read-only snapshots of the line and of the folder, sync          | 7.4 (7.3 to 10.0)  | 6.8 (6.7 to 8.3)   |
| assembly: snapshot, `rmdir` of the placeholder, snapshot into it, sync   | 8.3 (8.2 to 8.7)   | 7.8 (7.1 to 8.4)   |

### 5.7 Owners of a change list

Two commits, made in a writable snapshot of the line. The typical one: 40 changed files, 5 new
files (2 of them in `secrets/`, which a routing pattern matches), 5 deleted and 2 moved: 54
files and 64 paths with the folders. The large one: 1,000 changed files and the same others:
1,014 files and 1,025 paths. The stack has three overlays (10, 1,000 and 10,000 files), each
with five routing patterns. The owner rule is the one of 3.2: an `lstat` in the head state of
each overlay from the top, then the patterns.

| Step                                                    | typical (54 files) | large (1,014 files) |
| ------------------------------------------------------- | ------------------ | ------------------- |
| `btrfs send --no-data -p` (layr does this today)        | 5.4 (5.3 to 6.3)   | 28.8 (27.4 to 33.0) |
| read the paths from `btrfs receive --dump` (Python)     | 0.6 (0.6 to 0.7)   | 10.4 (10.2 to 10.5) |
| find the owner of each file (Python)                    | 0.3 (0.2 to 0.9)   | 4.0 (3.9 to 7.8)    |

Owners in the typical commit: shared set 9, top overlay 3, middle 1, bottom 41. The Python
times are an upper limit for the same work in Rust. layr already parses the stream in Rust.

## 6. Folder overlays and common programs

The test (`tools.sh`) makes a line with a nested subvolume `sec` and 1 MiB files, so that no
file is inline. "Reflink" means that the copy and the original start at the same physical
extent (`filefrag -v`).

| Action at the edge of the folder overlay                                     | Result                                                        |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `rename()` of a file from the line into the folder, and back                 | EXDEV (Invalid cross-device link)                             |
| `link()` into the folder                                                     | EXDEV                                                         |
| `rename()` of the folder itself inside the line                              | works                                                         |
| reflink (`FICLONE`, `cp --reflink=always`) across the edge                   | works, extents shared                                         |
| `cp` with default options (GNU 9.7, uutils 0.10)                             | works, reflink                                                |
| `mv` of a file or a folder, GNU coreutils 9.7                                | works: copy with reflink, then delete                         |
| `mv` of a file, uutils coreutils 0.10                                        | works, but copies all data (no shared extents)                |
| Python 3.14 `shutil.move`, `shutil.copy2`                                    | works, reflink                                                |
| save in the folder: temporary file in the folder, `fsync`, `rename()`        | works; `sed -i` and `vim` (`:wq`) work, the folder stays a subvolume |
| save with the temporary file outside the folder                              | EXDEV                                                         |
| `git status`, `add`, `commit`, `checkout` of branches that change files in the folder, `stash` | work, the folder stays a subvolume          |
| `git mv a.txt sec/`                                                          | fails: `fatal: renaming 'a.txt' failed: Invalid cross-device link`, exit 128 |
| `git checkout` of a commit without the folder, and `git rm -r sec`           | deletes the subvolume; a later checkout makes a plain folder  |
| the folder made a subvolume again (reflink copy of its files)                | `git status` stays clean                                      |
| `rm -rf sec` by a normal user who owns it, mount without `user_subvol_rm_allowed` | deletes the subvolume (`rmdir` of an empty subvolume)    |
| `btrfs subvolume delete sec` by that user                                    | refused: Operation not permitted                              |
| `find -xdev`                                                                 | stops at the folder (another `st_dev`): 3 of 13 files         |
| read-only snapshot of the line                                               | the folder is an empty placeholder (inode 2), not in `btrfs send`; `rmdir` removes it in a writable snapshot |

## 7. Open questions

1. **Delete of a path that a lower layer also has.** A user deletes `.env`, which is in an
   overlay and in the shared set. The commit deletes it from the overlay, and the next assembly
   shows the shared `.env` again. overlayfs keeps a whiteout for this. Does an overlay need a
   delete marker, or does the delete go to every layer that the user may write?
2. **Owner of a moved file.** A file of an overlay moves to a path that no pattern matches. Does
   it stay in its overlay (the owner follows the file; btrfs reports moves by inode) or go to
   the shared set (the owner follows the path)? Following the file is safer: a move never
   publishes a private file.
3. **Ignore rules.** `.env` is usually ignored, and layr does not commit ignored files. A path
   that a routing pattern sends to an overlay must be tracked there. Do routing patterns win over
   ignore rules, and do ignore rules apply per layer?
4. **The identity of a reviewed result.** `merge --expect`, approvals and check results name one
   state today. With overlays a head is a set of states. A proposal: a stack record (the shared
   state and the overlay states) with its own id, which `--expect`, approvals and checks name.
5. **Readers of assembled states.** The base and the automatic states (`save`, before every
   changing command, every 10 minutes) hold the files of all layers. Today the read right of
   the project covers every state. Such states must be readable only by the readers of all their
   layers, or automatic states must be split by layer too (one more snapshot per layer).
6. **Review by a user who does not read an overlay.** Does `diff main...topic` hide the overlay
   paths, show only their names, or refuse? The merge rule already needs a reader of all follow
   overlays.
7. **Shared mode and two lines.** With one overlay line for all lines of a user, a commit in
   line A moves the overlay line, and line B still has the old files. The next commit of B must
   not undo A's change without notice: refuse when the overlay head moved, or merge it in?
8. **Where a mode is set.** The owner sets the default and a line overrides it. Is that an
   option of `overlay create` and `overlay use`, or a separate command?
9. **A lost folder overlay.** `git checkout`, `git rm -r` or `rm -rf` deletes the nested
   subvolume (`rm -rf` was also tested as a normal user), and git can make a plain folder
   there. The next commit
   would then put the folder's private files into the shared set. layr must refuse a commit when
   the path of a folder overlay is not a subvolume, and offer a repair, as `layr cache repair`
   does for caches.
10. **Moves across the edge of a folder overlay.** `rename()` gives EXDEV: `git mv` fails, GNU
    `mv` copies with reflinks, uutils `mv` copies all data. A file overlay has no such edge. Are
    folder overlays worth it, or only for large private folders where O(1) assembly matters?
11. **Programs that stay on one file system.** `find -xdev` stops at a nested subvolume. Backup
    and copy programs with a one-file-system option probably skip folder overlays too (not
    tested).
12. **A file that was pushed.** `layr overlay move` of a file that is in a pushed shared state
    does not remove it from git: the old commits keep it. Should the command warn, or refuse
    without a flag?
13. **age keys.** Where do the public keys of readers live (records in the project database?).
    A reader who is removed can still read old encrypted backups.
14. **Replication.** Readers are users of a machine, as roles are. Which overlays does
    `layr sync` send to a peer: none, all encrypted, or only to machines with the same readers?
15. **Checks and private files.** A check runs on the full result as the caller, who reads all
    follow overlays. A check that copies its tree to another place can copy private files there.

## 8. What to build first

1. **File overlays in follow mode, with the apply commit.** Records: an overlay (owner,
   readers, patterns), the stack of a line, the stack head. Commands: `overlay create`, `route`,
   `move`, `use`. Reader checks on `show`, `diff`, `grep`, `blame`, `log`, `try`, `export`.
   Push of the shared set only, with a test that no overlay file reaches git. This covers the
   common case (`.env`, keys) and keeps a commit at about one snapshot per changed layer.
   Decide questions 1 to 5 before this step.
2. **Merge across follow overlays**, with the stack record for `--expect`, approvals and checks.
3. **Assembly** for lines with another stack (a line of another user, `overlay use`).
4. **Ephemeral mode**, then **shared mode** after question 7 is decided.
5. **Encrypted backups** with age.
6. **Folder overlays** last, and only for large private folders. They need the subvolume check
   of question 9.

Not measured: a cold page cache (the VM was shared, so caches were not dropped); a large file
system under write load; a Rust version of the owner lookup and of apply; apply with many
changed files (at about 14 µs per reflink copy, 1,000 files add about 14 ms); age encryption;
merges with overlays; stacks of more than three overlays.

## Appendix: scripts

All scripts are in `docs/overlays-bench/` and run as root on Linux with btrfs:

| Script        | What it does                                                                          |
| ------------- | ------------------------------------------------------------------------------------- |
| `setup.sh`    | Makes the loop file system, copies the tree into the line, picks the overlay files.  |
| `bench.sh`    | Times sections 5.2 to 5.7 and prints one TSV row per step and run.                   |
| `classify.py` | Reads a `btrfs receive --dump` listing and finds the owner of each file (5.7).        |
| `summary.py`  | Prints the median and the range of each step from the TSV rows.                      |
| `tools.sh`    | Runs the tests of section 6, some of them as a normal user.                          |

```sh
W=/root/ovl-bench docs/overlays-bench/setup.sh <source folder>
docs/overlays-bench/bench.sh 5 <folder> <folder> >bench.tsv   # folders for 5.6
docs/overlays-bench/summary.py <bench.tsv
docs/overlays-bench/tools.sh <uid of a normal user>
umount /root/ovl-bench/mnt && rm -rf /root/ovl-bench          # clean up
```

The two folder overlays of 5.6 were folders of the tree with 185 and 8,117 files.
