# Object store completion, 2026-11-08

Source line: `object-store-completion`, based on `linux-views-parity` (`3dbe06e`).
`source-sha256.json` identifies the source files for this result.

## Contract

After migration, source states remain usable without native state snapshots.
New saves write objects before record publication. Native copies can remain while a reader or active virtual view needs them.
Cache and derived states retain their native format. Native-only consumers can create disposable copies.
This change does not change the object encoding or enable objects in unmigrated projects.

## Runtime checks

| Environment | Object lifecycle | Virtual line lifecycle after migration |
| --- | --- | --- |
| macOS arm64, APFS | PASS | PASS, FSKit |
| Linux arm64, Btrfs | PASS | PASS |
| Linux arm64, OverlayFS | PASS | PASS |

`tests/object-state-parity.sh` covers migration, show, status, partial save, diff, restore, line creation, export, reset, undo, park, merge, and fsck.
Additional runs check `save --allow-empty` with no stage, followed by show, fsck, and a report of zero native source snapshots on all three backends.

It also checks hard links, symbolic links, executable mode, ignored paths, cache saves, and line creation with caches.
The service reports zero native source snapshots after migration, new saves, and history operations.
The check uses the service report because Linux state directories belong to the service and revision display IDs are not directory names.

`tests/views-parity.sh` runs with `LAYR_TEST_OBJECT_STATES=1`.
It checks saves, hide/show, conversion to a folder, metadata, ignored files, and deletion.
The Linux runs also exercise the existing failure injection for interrupted moves and retry.
Active macOS lower trees remain protected from native cleanup.

Linux runs use the private `layr-parity` Lima machine (Ubuntu 25.10).
The test caller has UID 501. Only the store services and mount setup run as root.
The guest has no virtiofs or 9p mounts from the host.

## Unit and compatibility checks

- macOS binary suite: 375 passed, 50 ignored, zero failures.
- Object reader tests: four passed on macOS and Linux.
- Commit recovery tests: two passed on macOS and Linux.
- `t202-migrate.sh`: 59 passed on a case-sensitive APFS volume.
- `t200-objects.sh`: 34 passed on a case-sensitive APFS volume.
- Linux Btrfs tests compile with `cargo zigbuild --tests --target aarch64-unknown-linux-musl`.
- Both Linux backend binaries compile with the same target; OverlayFS uses `--features overlayfs`.

The recovery test injects an error after the database commits.
It checks that content survives native cleanup, that a pin postpones cleanup, and that a repeated cleanup preserves content.
Reader tests check exact tree and times IDs after another object write, cross-directory hard links, and visible errors for corrupt blobs.
The final macOS reader test also checks that a corrupt object base cannot silently lose an entry during a save.

The full macOS suite uses `GIT_CONFIG_GLOBAL=/dev/null`.
An earlier run inherited the host signing configuration and failed the Git signing fallback test.
That test passed with isolated configuration; the complete isolated suite then passed.

## Reproduce

```sh
GIT_CONFIG_GLOBAL=/dev/null cargo test --offline -p layr --bin layr
LAYR=/path/to/layr LAYR_ROOT=/private/test/store bash tests/object-state-parity.sh
LAYR=/path/to/layr LAYR_ROOT=/private/test/store LAYR_TEST_OBJECT_STATES=1 bash tests/views-parity.sh
```

On Linux, set `LAYR_SOCKET` to the matching backend service and `TMPDIR` to a private writable directory.
Use a new isolated project store. Do not run the scripts as root.

## Limits

These results cover arm64 runtime behavior. They do not establish x86_64 runtime behavior or new performance numbers.
This thread did not publish to Layr Cloud, deploy a service, or create a release.
The release owner receives the saved Layr state for integration.
