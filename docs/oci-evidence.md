# OCI workstream evidence

Base: `38ea7d2045d063552f6ce0fb68b117ebf19a71ed` (`cloud/integration`).
Branch: `cloud/oci`.

## Phase A

Implemented:

- Root `layr.toml` schema, validation, and runnable selection.
- One deterministic tar/gzip layer per path set, with a content cache in `meta/oci`.
- OCI Distribution push with Basic and Bearer authentication and blob existence checks.
- Read-only registry in the existing daemon, enabled by project `registry.listen`.
- Per-user tokens, expiry, revocation, project ACL checks, and explicit anonymous access.
- Line and state tags, plus a single resolver hook for Core refs and changes.
- Deterministic Helm artifacts with the exact image digest in `values.yaml`.
- README schema, commands, paths, authentication, and cache details.

## Checks

Native macOS: 7 OCI tests passed, 0 failed.

The format checks for changed files and `git diff --check` pass.
The full `cargo fmt --check` reports pre-existing format differences in `apfs_stream.rs` and `overlayfs.rs`.

```sh
cargo test oci -- --nocapture
rustfmt --edition 2021 --check src/oci.rs src/oci/client.rs src/oci/registry.rs src/cmd/oci.rs src/cmd/admin.rs src/cmd/remote.rs src/service.rs
git diff --check
```

The deterministic fixture produces this digest on macOS and Linux:

```text
sha256:55ed1d514d29902527747dc430ad4642f9ff76739d57ff6be6aa0511fc90429e
```

The fixture uses different file times and directory creation orders.
The native process and the Linux process also use different file owners.
The tests inspect tar owners, timestamps, entry order, gzip headers, and cache reuse.

Linux build and tests use a separate copy in the OrbStack machine `layr-dev`:

```sh
COPYFILE_DISABLE=1 tar --no-xattrs --exclude=target --exclude=.git --exclude=__pycache__ -cf - . |
  orb -m layr-dev -u root bash -lc 'tar -xf - -C /root/work/oci'
orb -m layr-dev -u root bash -lc 'cd /root/work/oci && cargo test && cargo build'
orb -m layr-dev -u root bash -lc 'cd /root/work/oci && sudo tests/run.sh t30-oci t03-git t13-access t07-service'
```

Recorded Linux results:

- `cargo test`: 30 passed, 0 failed.
- `cargo build`: passed.
- `t30-oci.sh`: 24 passed, 0 failed.

Recorded results from the affected existing suites:

- `t03-git.sh`: 55 passed, 0 failed.
- `t13-access.sh`: 78 passed, 0 failed.
- `t07-service.sh`: 30 passed, 0 failed.

The OCI suite starts Distribution 2.8.3 and a Bearer authentication proxy.
It uses skopeo 1.21.0-dev and Helm 3.17.3 to pull actual manifests, configs, layers, and charts.
The tests compare the external manifest digest with the Layr registry digest.
They also reject cached manifests after a secret-policy change.
A repeated push must report zero uploaded blobs.

## Pending Core integration

The lead requires Phase B after review and merge of Core:

- List tags from Core refs and changes.
- Register layer blobs as detached `derived` states with context `oci-layer-v1`.
- Set `derived_from = Some(source_state)` and root the cache through Core refs.
- Add live environment, release, and change tag tests.

The temporary cache currently has no Core derived-state records.
The root integration function is `oci::push(&Ctx, url, rev, Option<runnable>) -> Result<i32>`.
The temporary dispatcher in `cmd::remote::push` uses `cmd::oci::dispatch`.
The root entry point always prints JSON for the generic remote dispatcher.
Ref reads currently use the project's reader policy because Core has no separate ref ACL.
Exact views return `MANIFEST_UNKNOWN` until the Core resolver supports them.
The implementation packages existing files and does not execute compiler commands or enqueue builds.
OCI attestation referrers and eager packaging after a ref update are not part of Phase A.
