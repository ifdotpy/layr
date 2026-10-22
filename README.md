# layr

Local version control on btrfs, for many users and many working folders on one machine. Git and GitHub stay the remote
(push, pull requests, CI). Everything local is btrfs: each line (branch and worktree in one) is a
writable snapshot, each state (commit) is a read-only snapshot, change lists come from
the btrfs metadata-send ioctl in O(changes), and a merge copies files with reflinks.

The commands follow the git verbs and print git formats, so people and tools that know git use
layr without new habits. Output of `diff`, `status --porcelain`, `show`, `log --format` and `--stat`
matches git byte for byte for the same content (tests/t08-gitcompat.sh).

Supported backend: Linux (btrfs, kernel 5.6 or newer). The experimental APFS backend is described below. Unsafe code is limited
to `src/sys.rs` (btrfs ioctls, fork, user switching, the user database); everything else uses
`rustix` and the standard library, and the crate denies `unsafe_code` outside that module.

layr is a general tool: it knows Unix users, projects, lines, states and rules, and nothing about
the programs that use it. Programs built on top of it use only its public commands.

## Requirements

- Linux with btrfs for the store (a loop file is fine), `btrfs-progs`, `git`, `zstd`.
- Root for the service: snapshots into protected folders and `btrfs send` need it.

## Build and test

```sh
cargo build --release
sudo tests/run.sh            # integration tests: each file gets a fresh btrfs on a loop file
cargo test                   # unit tests
```

## Experimental OverlayFS backend

The Linux `overlayfs` feature adds a backend for a VM with ext4 or XFS and root mount permission.
See [the setup, transfer rules and limits](docs/overlayfs.md).
The default Linux build remains btrfs.
Working caches are ordinary directories, so tools can remove and recreate them with `rm -rf`.
Snapshots copy the upper layer and share unchanged lower layers.

### OverlayFS benchmark results

Measured on 2026-10-16 with release binaries in one Linux VM: 10,000 files, 10 edits, three trials per binary.
The comparison uses the previous full-tree implementation and the new delta snapshots. Times are medians.
Commands run as root without the service; these tests do not measure changes of branch ownership.

| Filesystem | Operation | Full-tree snapshots | Delta snapshots | Speedup |
| --- | --- | ---: | ---: | ---: |
| ext4 | Clean status | 1069 ms | 395 ms | 2.7x |
| ext4 | Diff | 1095 ms | 276 ms | 4.0x |
| ext4 | Commit | 945 ms | 438 ms | 2.2x |
| ext4 | Merge | 3145 ms | 867 ms | 3.6x |
| XFS | Commit | 4675 ms | 2125 ms | 2.2x |
| XFS | Merge | 13279 ms | 3939 ms | 3.4x |

- ext4 private file allocation falls by 49.8%, from 492,957,696 to 247,443,456 bytes across three benchmark projects.
  This excludes directory and filesystem metadata. It is not the cost of one commit.
- XFS uses alternating runs. Its median `init` time rises from 10.3 to 19.1 seconds, an unresolved regression.
- VM latency varies greatly. Do not compare absolute ext4 and XFS times or treat these results as fixed backend limits.
- XFS can share reflink extents. Its file block counts do not establish physical space savings.
- Hard-linked trees and layer compaction still require full copies. The upper layer can contain changes from several commits.

See the [full report and raw samples](benchmarks/overlayfs-delta-20261016/README.md) for commands, hashes, all results and test coverage.

### Disk use compared with Git worktrees

**16 branches with private caches. About 94% less disk space.**
Layr on btrfs: **153 MiB**. Git worktrees on btrfs with full cache copies: **2508 MiB**.
These totals include the main workspace. The measured fixture uses 128 MiB of cache data per workspace.

Two runs use the same synthetic project: 2,048 source files, one main workspace, and 16 branches with one edit and commit each.
The cache case adds 128 MiB of private mutable data per workspace. Totals include history and additional filesystem metadata.

| Scenario | Layr | Git worktree |
| --- | ---: | ---: |
| Source only, btrfs | 22 MiB | 325 MiB |
| Source only, OverlayFS on ext4 versus Git on ext4 | 33 MiB | 283 MiB |
| Source only, OverlayFS on XFS versus Git on XFS | 19 MiB | 301 MiB |
| Private caches, btrfs, full Git cache copies | 153 MiB | 2508 MiB |
| Private caches, btrfs, reflink Git cache copies | 153 MiB | 455 MiB |

Layr on btrfs uses about 94% less space than Git with full cache copies in this fixture.
With reflink cache copies, the reduction is about 66%.
Git worktree does not populate ignored caches itself; the benchmark copies them explicitly.
Shared package-manager caches can change the result.

The current OverlayFS cache path copies data even on XFS: 2324 MiB versus 432 MiB for Git with reflink cache copies.
The source-only result does not establish the same benefit for private caches.
See the [method, full comparison and both raw runs](benchmarks/workspace-disk-20261016/README.md).

## Experimental APFS backend

APFS uses whole-directory clones automatically. There is no mode flag.
Trees with hard links or nested layers use file-by-file clones to preserve their behavior.
See the [10,000-file measurements](benchmarks/apfs-performance-20261015.md) for time and space costs.


The macOS build uses APFS. The Linux build continues to use btrfs with the existing store and record formats.
The store and signed record formats remain compatible with the Linux backend.

- The store requires a case-sensitive APFS volume.
- Commands run as the current user by default. Set `LAYR_SOCKET` to use the root service.
- The service requires a volume with ownership enabled and a separate root-owned store.
  It refuses volumes mounted with `noowners` and store folders that other users can change.
- A snapshot clones each file and checks source metadata before and after the copy.
  It fails if the source changes during the copy. It is not an atomic volume snapshot.
- APFS work grows with the number of files. Files with the same valid APFS clone identity need no content comparison.
  Other files require a content comparison to find changed blocks.
- `status` checks kernel change counters before and after a live read. It uses a temporary snapshot if validation fails.
  Files with an active memory mapping or an unavailable counter also use the snapshot path.
- `add` clones changed files into the private index instead of copying all bytes. Hard-linked index files use the existing copy path.
- `add` validates the working tree before and after it copies selected entries. It avoids full temporary snapshots when validation succeeds.
  The first `add` still creates a complete index from the head. Active `mmap` files use the snapshot path.
- A regular `commit` moves the prepared index into the state store and makes it immutable.
  A journal restores the index after a process stops before the database transaction completes.
  Commits with overlays retain the existing path.
- File modes, symbolic links, hard links and file identity survive snapshots. Nested layers stay outside history.
- Immutable flags prevent ordinary writes to states. The store owner can remove these flags.
  Use the root service for separate users and root-owned state storage.
- Overlays, `sync`, `serve-peer`, backup and restore are available.
- APFS sends full btrfs version 1 streams. Existing Linux peers can receive them without an update.
- APFS receives full and incremental streams. It copies clone ranges when APFS cannot share those ranges.
- APFS does not request compressed version 2 streams. Transfers between Linux peers retain their existing optimizations.
- A local restore gives the current user the admin role on the restored project.
- Transfers preserve file modes, hard links, symbolic links and extended attributes.
  macOS attribute names use a reserved `user.layr.apfs.*` namespace on Linux.
  Attributes of symbolic links use `trusted.layr.apfs.*`, because Linux forbids `user.*` attributes on symbolic links.
- Native access control lists cannot use the same rules on both systems.
  A transfer refuses macOS access control lists and Linux security attributes instead of silently losing their effects.
- Device nodes, sockets and named pipes are not supported by the APFS backend.
- APFS copies directory metadata, including access control lists and extended attributes.

Use a separate test store on an existing case-sensitive APFS volume:

```sh
cargo build --release
export LAYR_ROOT=/Volumes/YourCaseSensitiveVolume/layr-test-store
./target/release/layr init app --from ~/src/app
./target/release/layr --project app --line main branch topic
```

The core command tests also run on macOS. They require GNU coreutils and GNU sed in `PATH`.
`LAYR_TEST_DIRECT_MAIN=1` removes the default `main` policy in test projects.
The Linux tests use root and already bypass that policy. This option does not test access control.

```sh
export LAYR_APFS_TEST_ROOT=/Volumes/YourCaseSensitiveVolume
LAYR_TEST_DIRECT_MAIN=1 tests/run.sh t01-basic t02-merge t03-git t08-gitcompat
cargo test -- --ignored
```

The benchmark creates a new test directory and keeps its data and `results.json` for inspection:

```sh
python3 benchmarks/backends.py --binary target/release/layr \
  --root /Volumes/YourCaseSensitiveVolume/layr-bench-1000 --files 1000 --repeats 3
```

## Quick start

```sh
sudo layr daemon &                                   # the root service, /run/layr/layr.sock
sudo layr init app --from ~/src/app --owner alice    # alice is the admin; main is protected
layr access grant bob writer                         # (alice) roles: reader, writer, maintainer, admin
layr branch topic-1 main                             # (bob) a new line: O(1), its own folder
cd /var/lib/layr/projects/app/lines/topic-1          # a line is a folder
layr status; layr diff; layr add -A; layr commit -m "change"
layr merge topic-1 --expect <state alice reviewed>   # (alice, in main) exactly the reviewed state
layr push origin main                                # every state of main becomes a git commit
```

The store lives under `$LAYR_ROOT` (default `/var/lib/layr`). To work on a line from another
path, link it: `ln -s /var/lib/layr/projects/app/lines/topic-1 ~/code/topic-1`. Users other than root talk to the
service; root can run every command directly.

## Concepts

| Term      | Meaning                                                                                     |
| --------- | ------------------------------------------------------------------------------------------- |
| state     | read-only snapshot + signed record (id, parents, author, message, conflicts). A commit.    |
| line      | writable snapshot at `projects/<p>/lines/<name>` with an owner and a head state             |
| auto state| a snapshot of the working content: before every changing command, on `layr save`, and every 10 minutes when the content changed (the service) |
| cache     | a folder of regenerable content (such as a Rust `target`) kept as a nested subvolume: outside states, diffs and backups. A new line starts with a reflink copy of the nearest ancestor's cache (another user's only from a protected line; only one saved on the same platform, `<os>-<arch>`). Caches come only from `layr save --caches`; layr never keeps build output by itself. Cache folders: `layr cache add` and the folders that `.layrattributes` files mark `regenerable`; a folder with files in the head state is no cache |
| session   | a free-text label of the program that runs layr (`LAYR_SESSION`): shown in `op log` and used by `undo` to find "my last operation"; layr never decides permissions by it |
| record    | one signed operation (Ed25519, UUIDv7 ids) in the project database; undo restores pointers |
| role      | reader, writer, maintainer or admin of a user, a Unix group or everyone, per project and machine |
| protected line | a line (pattern) that changes only through merges of reviewed states, after its checks pass on the merge result |
| overlay   | files with their own owner, readers and history, shown in the lines of their readers above the shared set; `push`, exports, backups and every state of the project hold the shared set only (`docs/overlays.md`) |

States are folders: `layr show <state>:<path>` and `layr grep <pattern> <state>` read them
without a checkout.

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
## Runnable snapshots and OCI registries

Open Container Initiative (OCI) images come from immutable snapshot files.
A push never reads uncommitted files and never executes a build command.
Put the runnable definitions in `layr.toml` at the project root:

```toml
[runnable.app]
layers = [["runtime"], ["deps"], ["app", "config"]]
entrypoint = ["/runtime/bin/python"]
cmd = ["/app/server.py"]
env = { MODE = "production" }
working_dir = "/app"
ports = ["8080/tcp"]
platform = { os = "linux", arch = "amd64" }
chart = "deployment"
```

| Field | Meaning |
|---|---|
| `layers` | Required ordered list of path sets. Each set produces one OCI layer. An empty set produces an empty layer. |
| `entrypoint`, `cmd` | Argument arrays. At least one array must contain an argument. No shell interprets these arrays. |
| `env` | Map of environment names to string values. Default: empty. |
| `working_dir` | Absolute path inside the container. Default: `/`. |
| `ports` | Exposed ports, such as `8080/tcp` or `5353/udp`. Default protocol: `tcp`. |
| `platform` | Required OCI `os` and `arch`. Use `linux` and `amd64` or `arm64`, for example. |
| `chart` | Optional chart folder in the same snapshot. `Chart.yaml` must use the runnable name and a SemVer version. |

Paths are relative to the snapshot root.
A path can name a file, folder, or glob (`*`, `?`, `[...]`).
The path `.` selects all shared files except `.git`.
Paths cannot be absolute or contain `..`.
Unknown runnable fields and missing paths produce errors.
Each runnable has one platform.
Use a separate runnable name for each platform.

A structured layer can use `{ paths = ["app"], private = false, secret = false }`.
OCI export rejects a layer when `private` or `secret` is true.
Export also rejects overlay projects, states from lines with attached overlays, and paths in the project setting `secret.paths`.
The setting `oci.secret_paths` adds OCI-specific exclusions.
Special files and OCI whiteout names (`.wh.*`) are rejected.
Symbolic links are archived without access to their targets.

```sh
layr remote add images oci://registry.example/project/app
layr push images main --runnable app --json
```

The remote URL names the image repository.
With one runnable, `--runnable` is optional.
For credentials, set `LAYR_OCI_USERNAME` and `LAYR_OCI_PASSWORD` in the command environment.
The client also reads `remote.<name>.username` and `remote.<name>.password`.
The client supports Basic and Bearer authentication through the [OCI Distribution API](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md).
HTTPS is the default.
For a local test registry, set `remote.<name>.insecure` to `true` to use HTTP.

A layer uses sorted tar entries, timestamp zero, owner and group zero, and fixed gzip settings.
File modes, link targets, paths, and file bytes determine its identity.
Symbolic-link modes are fixed at `0777` because host systems report different modes for identical links.
The cache in `meta/oci` uses content hashes.
This temporary cache stores blobs outside Core derived states until the Core workstream supplies that integration.
A repeated export reuses compressed blobs.
A push checks each blob in the registry before upload.
The JSON result includes `state`, `runnable`, `image`, `chart`, `tag`, and `uploaded_blobs`.

A chart uses the same revision as its image.
Layr sets `image.digest` in `values.yaml` to the exact image manifest digest.
Chart templates must use this value, for example `{{ .Values.image.repository }}@{{ .Values.image.digest }}`.
Layr uses the [Helm OCI config and chart layer media types](https://helm.sh/docs/topics/registries/).
A push writes the chart to the sibling repository `project/charts/app`, with its chart version as the tag.
Helm replaces `+` with `_` in a version tag.
Chart archives have fixed metadata and sorted entries.
Chart folders cannot contain symbolic links or paths excluded as secrets.

To enable the read-only registry in the Layr service:

```sh
layr config registry.listen 127.0.0.1:5080
layr config registry.public_url https://layr.example
layr access token alice --until 1d
# Restart the Layr daemon after a registry.listen change.
```

The token command prints one opaque token and records only its SHA256 hash.
A new token replaces the user's token on this machine.
`layr access token --revoke alice` revokes that user's token on this machine.
The default token lifetime is one day.
Token issue and revocation require the `access` permission.
The token belongs to one user and one Layr machine.
Use a TLS reverse proxy for access outside the local machine.
Set `registry.public_url` to the public origin so Bearer challenges use the correct token endpoint.
The listener itself speaks HTTP.

The service accepts a Bearer token on each request.
It also offers `/token` for the Basic credential exchange that Helm and skopeo use.
Use any username and the issued token as the password for these clients.
The service checks the user's current project reader policy on each request, including requests for cached blobs.
The current policy applies to every ref in the project.
The authorization function is the extension point for future per-ref policies.
Anonymous access requires both `registry.anonymous = true` and the project's `*` reader role.
Anonymous access is off by default.

The endpoints support `GET` and `HEAD` for manifests and blobs, plus tag lists with `n` and `last` pagination.
All write requests fail with HTTP 405.
Image paths use `<project>/<runnable>`.
Chart paths use `<project>/charts/<runnable>`.

| OCI tag | Layr revision |
|---|---|
| `line-main` | Line `main` |
| `state-<id>` | Exact state `<id>` |
| `prod`, `staging` | `env/prod`, `env/staging` |
| `1.4.2` | `release/1.4.2` |
| `change-<id>` | `changes/<id>` |
| `view-<id>` | `views/<id>` |

Ref and change tags use the Core revision resolver.
An unavailable exact view returns `MANIFEST_UNKNOWN`.
A manifest digest remains readable after the service has generated or exported that manifest.

```sh
skopeo copy --src-creds alice:TOKEN docker://layr.example/project/app:line-main dir:./image
helm pull oci://layr.example/project/charts/app@sha256:DIGEST --username alice --password TOKEN
```

The test file `tests/t30-oci.sh` needs Linux, Btrfs, `docker-registry`, `skopeo`, `helm`, `htpasswd`, `curl`, and Python 3.
It checks external image and chart pulls, Basic and Bearer push, service pulls, token revocation, and secret rejection.
