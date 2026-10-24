# APFS features for the Layr backend

This note reviews seven APFS features that the backend did not use, and the decision for each.
Measurements: [benchmarks/apfs-features-20261023](../benchmarks/apfs-features-20261023/README.md).

## Used before this change

- File and folder clones (`clonefile`, `fclonefileat`).
- Clone IDs (`ATTR_CMNEXT_CLONEID`): equal clones need no content comparison.
- Change counters (`ATTR_CMN_GEN_COUNT`): `status` reads the live folder without a copy.
- `RENAME_EXCL` and `RENAME_SWAP`, `UF_IMMUTABLE`, creation times, sparse clones.

## Review

| Feature | Decision | Reason |
| --- | --- | --- |
| 1. `getattrlistbulk` | Used | One call per folder instead of two per entry. `status`, `add`, `save`, `save -m <message>` and `diff` use 30 to 40 percent less CPU. |
| 2. FSEvents | Not used | A write through a memory mapping gives no event, so a status from events can be out of date. A saved status with stat data replaces it: about 5.5 times less CPU for `status` at 100,000 files (below). |
| 3. `F_LOG2PHYS_EXT` | Used | Blocks of two clones at one physical address need no read. A 4 KiB change in a 1 GiB file: 0.62 s to 0.03 s per command. |
| 4. Volume snapshots | Not used | `fs_snapshot_create` needs root and an Apple entitlement (`man fs_snapshot_create`). A normal user gets EPERM. |
| 5. One APFS volume per project | Not used | It needs an administrator. Clones cannot cross volumes. Its main use, snapshots, needs item 4. |
| 6. Transparent compression | Not used | There is no public API to compress a file in place. `ditto --hfsCompression` makes a clone on APFS. Compression would also stop block sharing with the lines. |
| 7. Document IDs | Not used | The test volume gave no ID after `UF_TRACKED`. The flag is one more write per file. The creation time already serves as the identity. |

## Details and limits

### 1. Bulk scans

- Asking for `ATTR_CMN_OBJTYPE` makes the kernel drop the setuid bit from the access mask. The type comes from the mask instead.
- The folder group of attributes comes only for folders, the file group only for other objects.
- A unit test compares the bulk scan with the previous `lstat` scan for files, links, hard links, read-only and setuid files, Unicode names and nested layers.

### 2. FSEvents, and the saved status that replaces it

FSEvents can replay the volume event log since a stored ID, and a first version used it for `status`
(100,000 files: 0.01 s CPU instead of 3.5 s). It was removed before release, because:

- A write through a memory mapping gives no event at all. A status from events could miss it.
- Volumes without an event log (images attached with `-nobrowse`) need a full comparison anyway.

An out-of-date status is not acceptable, so `status` uses a saved status with stat data instead, as git's index does:

- Each status saves its result and, for each entry of the working folder, the inode, mode, size, times,
  kernel change counter, and clone ID, from a scan made before the read.
- The next status scans the folder once with `getattrlistbulk` and compares only the entries that differ
  with the index. A write changes the change counter, also through a memory mapping or through a hard link outside the folder.
- A mapped file has no change counter while it is mapped: it is compared again at each status.
- A new head or index, a changed `.gitignore` or `.layrignore`, or more than 20,000 changed paths compare all.
- After few changes the saved baseline stays; the same paths are compared again next time. From 256 changed paths on, the baseline is saved again.
- `LAYR_NO_STATUS_CACHE=1` compares all for one command.
- Limit, shared with the full comparison: bytes written through a mapping that is unmapped without `msync` and not yet written back
  do not change the change counter or the clone ID, so neither method reads them until the kernel writes them.

100,000 files, one or two changed files: 0.22 to 0.24 s CPU instead of 1.28 to 1.35 s. The single folder scan is most of it.

### 3. Physical extents

- Files of 1 MiB or more and of equal size use the mapping. Others are read in full.
- Both files are flushed first: data in the page cache can still map to the old shared block.
- Only blocks at different addresses are read and compared, so the result stays exact.

## Copies while files change

A snapshot used to fail when a file changed during the copy. Now:

- A complete copy copies again only the entries that changed, until a scan sees no change.
  A tree with hard links is copied again as a whole, so its links survive.
- A thin layer computes the difference again and writes the layer again.
- A failure because a file or folder went away is a change, not an error.
- After 6 rounds with changes, the last copy stays with a warning. Each file is then one complete clone from some moment of the copy, as with `git add`. Entries that went away during that copy are removed.
