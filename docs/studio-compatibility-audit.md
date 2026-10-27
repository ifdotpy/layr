# Studio compatibility audit

Source: `origin/main` at `ec8ba0fb17389e5064045b07961be84c7e422ba0` (package 0.4.0).
Comparison: `integ/layr-0.2.1` at `c9c05499fa6332eed63f5a332b8a976ab61ed71b`,
`integ/layr-0.2.2` at `87ef4f80afd6609d9c5ec9f40d55ce95f079bb69`,
and `integ/layr-0.2.3` at `6dd2be0f8f97b345f99dcde905df70d263f0e2c9`.

`git log origin/main..integ/layr-0.2.3` contains eight unique commits.
The earlier integration branches contain subsets of these commits.
No complete equivalent of these fixes or regression suites exists in the inspected main revision.
This branch ports the fixes and keeps the 0.4.0 Git history import and public fetch behavior.

| Integration commit | Audit result and port | Regression evidence |
| --- | --- | --- |
| `803f7a6` | Missing: peer sync/records permission checks, source resolution under the merge lock, cache clones/hard links/nanoseconds, store root aliases. | `tests/upstream-fixes.py`: peer permissions, merge source race, root alias, cache identity/time tests. `apfs::tests::populated_cache_conversion_keeps_clone_identity`. Linux role cases in `tests/t13-access.sh`. Root alias also has open PR #23. |
| `10258b4` | Missing: permission/protection/check revalidation after merge checks, private store record permission checks, cache directory/file extended attributes and access control lists (ACLs), conversion error propagation. Public fetch retains read permission semantics. | `tests/upstream-fixes.py`: merge permission/protection/command changes, store permissions, directory mode/extended attribute/ACL tests, copy failures. Linux role cases in `tests/t13-access.sh`. |
| `c9c0549` | Missing: symlink attributes, private intermediate cache directories, incomplete copy cleanup, refusal of unreadable/special entries. | `tests/upstream-fixes.py`: pnpm links/shims, symlink attributes, failure/source preservation. `repo::cache_copy_tests::incomplete_cache_directories_are_private`. |
| `2c1c185` | Missing regression: unchanged Git folder import allocation. | `tests/git-init-space.py`: unchanged 128 MiB file allocation bound and later source edit isolation. |
| `e8a18ee` | Missing regression: modified Git folder import allocation. | `tests/git-init-space.py`: modified 128 MiB file allocation bound and historical HEAD bytes. |
| `2073752` | Missing regressions: tracked ignored paths, empty directories, directory modes. | `tests/git-init-metadata.py`: four public CLI cases. |
| `87ef4f8` | Missing: immutable source snapshot, exact HEAD blob comparison and clone, tracked ignored paths, directory metadata, replacement clones for modified files. Port threads the source through the complete 0.4.0 commit graph import. | Both Git import suites. Byte/type/mode cases include same-size edits, missing files, symlink parents, symlinks, executable files, and clean status. |
| `6dd2be0` | Missing: temporary owner write permission during thin directory metadata copy, final mode restoration, failure context. | `thin::tests::readonly_directory_metadata_keeps_modes`, with source mode 0555 and file mode 0444. The test fails on main with Permission denied. |

The integration package version changes (0.2.1, 0.2.2, 0.2.3) are not ported.
The target package remains 0.4.0.

## CLI changes from 0.2.3 to 0.4.0

The table compares source at the revisions above.
It does not assert local store format compatibility.

| Caller surface | Change |
| --- | --- |
| Core commands and global flags | No renamed core commands or removed global flags. `--root`, `--project`, `--line`, and `-C` remain. |
| `LAYR_DIRECT`, `LAYR_ROOT` | Same semantics. `LAYR_DIRECT=1` forces direct mode. macOS uses direct mode without `LAYR_SOCKET`. `LAYR_ROOT` selects the local store. |
| `checks` | Previously returns 1 with a removal error. Now executes checks. `checks.runner` and `checks.privileged-runner` require `config --local`. |
| New commands | `index write [remote] [--json]`, `request`, `requests`, `comment`, `release attach/files/get`. |
| New store flags | `push --index`, `clone --public`, `remote public NAME on|off`, `remote mode NAME private|service|public`, `remote rotate NAME`. Mode/rotate accept `--remove LOGIN` and `--reason TEXT`. Public on requests the project name on stdin. |
| Paper key | `remote add --paper-key-tty` creates a key through a confirmed private terminal. Automation uses `--paper-key-out PATH`. |
| Person identity | `person keys --store` adds store registration. Its output adds a signing key. A saved login can replace `LAYR_PERSON_LOGIN`. |
| JSON output | `change show --json` adds `checks` per version and `comments` at the top level. Core status, line, and view JSON source formats do not change. |
| Exit codes | Transfer status 2 for missing lines remains. Generic command error 128 and unknown command 1 remain. New checks commands define their own outcomes. |
| Access | Clone grants the local caller admin. Public fetch requires read permission. |

Sources: `src/main.rs`, `src/cmd/{mod,admin,remote,change}.rs`, and `src/person.rs`.
`src/args.rs` and `src/service.rs` do not change between the compared revisions.

## Related defects from review

Cache cleanup now grants owner write access to directories before deletion.
The new public regression uses 0555 directories and 0444 files.
Both first conversion and cache repair must remove the old tree and preserve the new modes.
Some macOS versions also require temporary owner write access on the directory during rename.
The replacement records its intent before the copy starts.
It records both directory identities and modes before it changes either name.
Renames use held parent descriptors and refuse a changed directory identity.
The next save or cache repair recovers an interrupted replacement.
Tests exit child processes during the copy, after permission changes, and between renames.
A concurrent `cache list` test must preserve the active repair journal and staging directory.
Read-only commands do not recover journals. Recovery requires the project lock.
They also inject a mode restoration error and replace the staging entry with a symbolic link.

The line root reserves `.layr-cache-replace-<64 hex digits>.json` and its `.tmp` companion for this journal.
These files are private and do not enter history.
A backup without a journal from an older binary requires explicit restoration of its folder.

Machine identity creation now has an exclusive local lease and create-once publication.
Main also has the original identity race, independent of the format guard.
`tests/machine-identity.py` uses 24 concurrent initializations for each fixture.
It checks fresh roots, marker-only crash recovery, and an existing partial ID.
Every project receives a later write and an `fsck` check.
