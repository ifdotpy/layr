# APFS backend compared with btrfs and git worktrees

This note lists where the APFS backend is weaker than the btrfs backend, apart from measured space and time.
It also compares Layr lines on APFS with `git worktree` on the same APFS volume.
See [thin APFS states](apfs-thin-states.md) and the [measurements](../benchmarks/apfs-thin-20261022/README.md).

## Where APFS is weaker than btrfs

1. **Snapshots are not atomic.** A btrfs subvolume snapshot is one kernel operation.
   On APFS, Layr scans the tree before and after a copy and copies again what changed.
   Under constant writes it keeps the last copy after 6 rounds, with a warning:
   each file is then one complete version from some moment of the copy, not one moment for all files.
   Files with an active `mmap` have no change counter, so status uses a slower complete copy.
2. **Work grows with the number of files.** A btrfs branch costs the same for any tree.
   An APFS branch is one complete tree copy: about 1 s CPU for 10,000 files and about 20 s for 100,000.
   `add` and automatic states scan the complete tree; btrfs gets changed paths from `send --no-data`.
   `status` scans once and compares only entries whose stat data changed ([APFS features](apfs-features.md)).
3. **Weaker protection of history.** btrfs read-only subvolumes are kernel-enforced.
   APFS uses `UF_IMMUTABLE` on each file. The store owner can remove these flags, and setting them costs CPU per file.
   Real protection needs the root service on a volume with ownership enabled.
4. **Weaker transfers.** APFS sends full streams only: no parent, no clone sources, no compressed version 2 streams.
   The receiver stores complete trees. btrfs sends only the changes.
5. **Move detection is a heuristic.** btrfs keeps inode numbers between snapshots.
   APFS uses the creation time. A tool that sets old times (tar, npm, zip) disables move detection for those files.
   A false match is unlikely, but possible.
6. **File system limits.** On the default macOS volume, which ignores case, names that differ only in case
   (`README` and `Readme`) cannot exist together; Layr refuses them with an error. A store on a case-sensitive APFS volume accepts them.
   Device nodes, sockets and named pipes are not supported.
   Transfers refuse macOS access control lists and Linux security attributes.
   `init --from` loses hard links (a known defect).
   APFS volume snapshots (`fs_snapshot_create`) return EPERM for a normal user.
7. **More code, more cases.** Thin states need views, keyframes in `gc`, delayed deletion of bases,
   and removal of layers without a record. btrfs gets all of this from the file system.
   - `rm`, `mv`, `apply --cached` and `restore --staged` first make a complete index.
   - Merges, imports, received states and lines with overlays keep complete trees.
   - Commands that need a folder (`show`, `merge`, `push`) make a view. It uses space until `gc` removes it.
   - A long chain of layers makes reads slower until `gc` makes a complete tree.
8. **Recovery.** A journal protects a stopped commit. Other partial states wait for `gc`.
   It removes a layer without a record only after one hour. On btrfs an incomplete snapshot never appears.

The two main limits are the non-atomic snapshot and the cost per file of each branch.
See [APFS features](apfs-features.md) for the features that were reviewed to reduce them.
Both need APFS volume snapshots, which need root and an Apple entitlement.

## Layr lines and git worktrees on APFS

`benchmarks/apfs-thin-20261022/worktree.py` runs on one APFS volume with git 2.54.0 and Layr `916a59d`.
It makes 4 worktrees or lines, changes 10 files in each, and runs status, add and commit.
Values are medians of the 4 lines. CPU is user and system time. Single run on a loaded host.

| 10,000 files of 4 KiB | git worktree | Layr line |
| --- | ---: | ---: |
| New worktree or line, space | 43.8 MiB | 3.5 MiB |
| New worktree or line, CPU | 1.03 s | 0.63 s |
| First status after the change, CPU | 0.33 s | 0.17 s |
| add, CPU | 0.12 s | 0.17 s |
| commit, CPU | 0.13 s | 0.10 s |
| commit, space | 0.02 MiB | 0.20 MiB |

| Same, plus one file of 1 GiB | git worktree | Layr line |
| --- | ---: | ---: |
| New worktree or line, space | 1083 MiB | 3.2 MiB |
| New worktree or line, CPU | 1.77 s | 0.86 s |
| First status after the change, CPU | 3.12 s | 0.23 s |

- A git worktree writes a full copy of every file. A Layr line clones the files: only metadata uses space.
- The first `git status` in a new worktree reads files to fill the stat cache. With a large file this costs seconds.
  Later git status calls are faster. This test measures only the first one.
- git `add` and a git commit are as fast or faster, and a git commit is smaller: git stores changed files as compressed objects.
- git keeps a second copy of all content in its object store. A Layr project keeps one copy, shared by clones.

### What a worktree does not have

- Automatic states of uncommitted work, `undo`, and retention.
- Warm caches for new lines (build output as clones), overlays, access rules, and the root service.
- Isolation: Layr keeps states in a store that line owners cannot change through the service.

### What Layr on APFS does not have, compared with a worktree

- Atomic reads of a folder under writes: git reads files one by one and does not need a snapshot,
  but it also does not promise a consistent copy. Layr fails a snapshot when files change.
- Wide tool support: every editor and tool knows git worktrees.
- Many lines of a large tree cost CPU per line in Layr (one clone). A worktree costs more space and more time per line.

On APFS, Layr is better than a worktree for space and branch time, and much better with large files.
It is equal or a little slower for `add`, and its commits use more space.
