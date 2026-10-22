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
| 1. `getattrlistbulk` | Used | One call per folder instead of two per entry. `status`, `add`, `save`, `commit` and `diff` use 30 to 40 percent less CPU. |
| 2. FSEvents | Used for `status` | A saved status is brought up to date for the changed paths only. 100,000 files: 0.01 s CPU instead of 3.5 to 4.1 s. |
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

### 2. FSEvents status

A short-lived stream replays the volume event log since the saved event ID. No daemon runs.

- The log can be off: images attached with `-nobrowse` have no `.fseventsd`. Such volumes read all at once.
- Each saved status writes its own marker file. The marker must come from the replayed history, before `HistoryDone`.
  A young event can arrive live without any log, so a live marker proves nothing.
- A cookie written after the stream starts must arrive. Events come in ID order, so every earlier change is in.
- Dropped events, hard links, a changed `.gitignore` or `.layrignore`, a new head or index, or more than 20,000 paths read all.
- A write through a memory mapping gives no event. A full read that finds a mapped file saves no status.
  A saved status serves for `watch.full_seconds` (default 60) after a full read. So a mapped write that starts after a full read can be missing from `status` for that time. `watch.full_seconds 0` turns the saved status off.
- `LAYR_NO_WATCH=1` reads all for one command.

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
