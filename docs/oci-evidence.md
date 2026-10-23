# OCI workstream evidence

Current base: `b8b2b1a` (`cloud/integration`, Core merge).
Phase A base: `38ea7d2045d063552f6ce0fb68b117ebf19a71ed`.
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
The Linux-only invalid-UTF-8 filename test cannot create its fixture on APFS.
APFS rejects the filename with `EILSEQ` before export.

The format checks for changed files and `git diff --check` pass.
The full `cargo fmt --check` reports pre-existing format differences in `apfs_stream.rs` and `overlayfs.rs`.

```sh
cargo test oci -- --nocapture
rustfmt --edition 2021 --config skip_children=true --check src/oci.rs src/oci/client.rs src/oci/registry.rs src/cmd/oci.rs src/cmd/admin.rs src/cmd/remote.rs src/service.rs
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
orb -m layr-dev -u root bash -lc 'cd /root/work/oci && sudo tests/run.sh t30-oci t18-core t13-access t07-service'
```

Final Linux results, 2026-10-23 UTC:

- `cargo test`: 40 passed, 0 failed (including 8 OCI tests).
- `cargo build`: passed.
- `t30-oci.sh`: 42 passed, 0 failed.
- `t18-core.sh`: 72 passed, 0 failed.
- `t13-access.sh`: 78 passed, 0 failed.
- `t07-service.sh`: 30 passed, 0 failed.

The combined command exits with status zero.
Studio monitor: `a104f61e-ce6b-5a4d-b90a-1d11abff4b77`.
Native monitor: `ceadf21b-a7fe-5901-af63-3294c7e7aed2`.
The native run has 7 OCI tests and produces the same fixed digest.
The Linux-only test attribute changes test selection on macOS, without a change to the Linux test or runtime code.
The earlier Phase A `t03-git.sh` run passed all 55 checks.

The OCI suite starts Distribution 2.8.3 and a Bearer authentication proxy.
It uses skopeo 1.21.0-dev and Helm 3.17.3 to pull actual manifests, configs, layers, and charts.
The tests compare the external manifest digest with the Layr registry digest.
They also reject cached manifests after a secret-policy change.
A repeated push must report zero uploaded blobs.

## Phase B and review corrections

Implemented after the Core merge:

- `Db::refs` supplies environment and SemVer release tags.
- `Db::changes` supplies change tags, with `revs::resolve` as the resolver.
- Layer blobs live in detached Core states of kind `derived`.
- Each state has context `oci-layer-v1` and `derived_from = Some(source_state)`.
- Refs `derived/oci-layer-v1/<blob-hash>` keep the layer states through garbage collection.
- Repeated content reuses the same derived state without a second compressed copy in `meta/oci`.
- The default push output contains human text. `--json` selects JSON.

The root daemon forks an HTTP child without reading its network connection.
The child permanently drops its uid, gid, and supplementary groups.
It executes the same binary in a clean environment before HTTP input.
This removes the daemon's key memory and unrelated file descriptors.
`registry.user` defaults to `nobody`.
User ID zero, group ID zero, and a failed privilege check prevent startup.
The unprivileged child sends a typed request through the existing service Unix socket.
The root backend checks the socket peer, token, and ACL.
It returns bounded metadata and manifest or blob bytes, without a network descriptor.
The HTTP child permits one request, a 4096-byte request line, and 16384-byte headers.
Read and write timeouts are ten and sixty seconds.

The service interval test waits for the daemon's existing readiness message before it measures the interval.
Its comparison counts and interval assertions are unchanged.
The malformed-request test reads the complete HTTP status line, because one TCP read can return a partial line.

The OCI integration suite checks the child's real, effective, saved, and filesystem user and group IDs in `/proc`.
It also checks that the child has no supplementary group zero.
The registry user cannot use the ordinary CLI protocol on the Unix socket.
It sends malformed and oversized HTTP requests, then checks that the daemon still serves valid requests.
It checks environment, release, and change tags, Helm release pulls, derived-state reuse, and Core garbage collection.

The root integration functions are `oci::push(&Ctx, url, rev, Option<runnable>) -> Result<i32>` and `oci::push_json`.
The generic remote dispatcher selects `push_json` only for `--json`.
The temporary hook in `cmd::remote::push` uses `cmd::oci::dispatch`.

Remaining limits:

- Ref reads use the project reader policy because Core has no separate ref ACL.
- Exact view tags return `MANIFEST_UNKNOWN` until Core supports exact views.
- Packaging does not execute compiler commands or enqueue builds.
- OCI attestation referrers and eager packaging after a ref update remain outside this task's explicit five goals.
- ECR and Lambda have no live test in this environment.
- The Wave 2 secret guard must call `secrets::check_history` before OCI push after that module is integrated.
