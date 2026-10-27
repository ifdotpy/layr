# Native workspace integration contract

## User flow

New Studio chats offer Layr VM, Layr Native, and Git Worktree.
Layr VM always stores projects on btrfs inside a Linux VM.
Layr Native runs the provider on the host. The main chat uses the selected project folder.
Workers use separate managed lines in an external store.
Git Worktree keeps the existing host implementation.
Existing chats keep their saved mode, including legacy image chats.

A native project has a small `.layr/link.json` file. History, keys, the database,
and worker lines stay outside the selected folder. The link is a locator, not
permission to read or write a store. The store keeps the authoritative binding.
The main folder must not move, become a symlink, or be replaced during attachment.
A normal user can remove the link without removing stored history.

## Required behavior

- Commands from the project root or a child folder resolve the same project and line.
- Source changes appear immediately to the user and the main agent.
- Worker changes stay in their own line until an explicit merge.
- Existing ignored files and the Git repository remain in the original folder.
- `.layr` never enters snapshots, diffs, Git exports, backups or worker copies.
- A link cannot redirect a privileged service to another user's store or folder.
- Changing or replacing a linked folder stops operations until explicit repair.
- Repeating attachment with the same identity returns the same binding.
- A crash cannot leave an apparently complete binding with no usable history.
- Line deletion, rename, restore and garbage collection cannot remove the input folder or its binding.

## Backend constraints

Managed worker lines keep their existing native snapshot mechanism.
An ordinary input directory is not necessarily a btrfs subvolume or an OverlayFS mount.
Its snapshot path therefore needs an explicit verified capture operation. Do not
silently treat it as a native subvolume, relocate it, or synchronize two mutable copies.
Capture must preserve hard links, sparse files, modes, links and supported attributes.
Concurrent source changes must fail capture before a state is published.
The capture check is not a claim of an atomic snapshot of concurrent writes.

The input filesystem can differ from the store filesystem. Case rules and metadata
support require validation before a merge or restore can change the input folder.
The slower capture cost of an ordinary input folder must not be presented as the
btrfs subvolume benchmark. Worker lines can retain the measured native behavior.

## Verification

Use disposable project folders on APFS, btrfs, ext4 and XFS. Keep the real project
folder and running Studio untouched. Check attachment, ordinary edits, commit,
branch, merge, restore, restart, interrupted attachment and forged links. Check
that the original folder's identity and ignored files survive all operations.
Then check the Studio creation, provider launch, worker creation and archive flows
through their actual API and UI callers. No unavailable mode may fall back silently.

## CLI example

Use a new project name and an external store supported by the selected backend.
The example uses a direct CLI session with access to that store.

```sh
export LAYR_ROOT=/absolute/path/to/store
layr init example --from /absolute/path/to/source
layr --project example protect main --remove
layr workspace attach example --path /absolute/path/to/source
cd /absolute/path/to/source
layr status
```

The input folder keeps its Git metadata. Layr commits do not change the Git head.
Worker lines stay in the external store.
For service access, use the configured `LAYR_SOCKET` and the source owner's identity.

## Current limits

Use the new CLI for every operation on an attached store.
Older binaries do not understand the workspace binding and can use the old internal line path.
Compatibility tests for ordinary stores do not establish compatibility for attached stores.

There is no detach or repair command yet.
Removing the locator does not detach the private binding.
An identity mismatch stops operations instead of adopting a replacement folder.
Case aliases that could overwrite another input name are rejected before file changes.
