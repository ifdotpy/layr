# Virtual-line parity, 2026-11-08

Source: line `linux-views-parity`, based on `layrfs-review` state `1f3805b`.
The state that contains this report also contains the tested changes.

The shared contract is [views-parity.sh](../../views-parity.sh).
Each attached log ends with PASS.

- macOS: native arm64 caller, uid 501, enabled FSKit extension.
- Linux: Ubuntu 25.10 arm64, Lima instance `layr-parity`, uid 501.
- Linux isolation: no host mounts, no shared home.
- Btrfs: loop volume with `user_subvol_rm_allowed`.
- OverlayFS: private store mount.
- Linux daemons run as root. All test callers run as uid 501.

The script verifies creation, staged status, diff with new files, save, ignored files,
executable mode, symlinks, and search through `layr tool rg`.
It verifies hidden status and save, repeated hide/show, conversion, and refusal of an occupied target.
After hidden deletion, it reads the final state through `op log` and verifies its bytes through `show`.

Linux tests inject a failure after the tree move, before the path-index update.
Delete and save refuse the unresolved move. A repeated view command recovers the files.
The external conversion test also verifies the Btrfs filesystem identity check across subvolumes.

Builds passed with Rust 1.99.1:
- `cargo build --offline -j4 -p layr`
- `cargo zigbuild --target aarch64-unknown-linux-musl --offline -j4 -p layr`
- `cargo zigbuild --target aarch64-unknown-linux-musl --features overlayfs --offline -j4 -p layr`

These results establish the tested command behavior.
Linux uses native Btrfs or OverlayFS trees and retains the Git bridge.
The tests do not establish equal speed, identical storage, x86_64 runtime support, or release integration.
