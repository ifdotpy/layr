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

## AWS deployment

Use the [AWS runbook](infra/aws/README.md) to deploy a store, gateway, and executor.

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
- Overlays and peer sync are available.
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
layr git push origin main                                # every state of main becomes a git commit
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
| cache     | a folder of regenerable content (such as a Rust `target`) kept as a nested subvolume: outside states and diffs. A new line starts with a reflink copy of the nearest ancestor's cache (another user's only from a protected line; only one saved on the same platform, `<os>-<arch>`). Caches come only from `layr save --caches`; layr never keeps build output by itself. Cache folders: `layr cache add` and the folders that `.layrattributes` files mark `regenerable`; a folder with files in the head state is no cache |
| session   | a free-text label of the program that runs layr (`LAYR_SESSION`): shown in `op log` and used by `undo` to find "my last operation"; layr never decides permissions by it |
| record    | one signed operation (Ed25519, UUIDv7 ids) in the project database; undo restores pointers |
| role      | reader, writer, maintainer or admin of a user, a Unix group or everyone, per project and machine |
| protected line | a line (pattern) that changes only through merges of reviewed states, after its checks pass on the merge result |
| overlay   | files with their own owner, readers and history, shown in the lines of their readers above the shared set; `push`, exports, remote copies and every state of the project hold the shared set only (`docs/overlays.md`) |

States are folders: `layr show <state>:<path>` and `layr grep <pattern> <state>` read them
without a checkout.

## Commands

| Group         | Commands                                                                                     |
| ------------- | -------------------------------------------------------------------------------------------- |
| work          | `status [--json]`, `diff`, `add`, `rm`, `mv`, `apply [--cached]`, `commit [-a] [--amend]`, `restore`, `reset`, `stash`, `clean` |
| history       | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`                        |
| lines         | `branch [<prefix> <rev> --count N] [--json]`, `tag`, `merge [--expect]`, `cherry-pick`, `revert`, `rebase`, `adopt`; `switch`/`checkout <line>` explain that lines are folders |
| access        | `access [list|grant [--until <30m|2h|1d>]|revoke|deny|allow|check]`, `protect [<pattern> --update <role> --check <names> --path <path>=<role> --approvals <n> --approvers <principals> --direct --rewrite <role> --no-expect | --remove]`, `approve [--withdraw] <state>`, `attest <kind> <rev> [--payload <file>]`, `attestations <rev> [--kind <kind>] [--json]` |
| safety        | `undo`, `op log`, `save`, `try <state> -- <cmd>`, `bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] [--json] -- <cmd>`, `gc`, `fsck`, `reindex` |
| Git bridge    | `git remote`, `git fetch`, `git pull`, `git push [--squash]`, `git clone`, `git sync`                             |
| remotes       | `remote add\|list\|self\|remove\|trust`, `sync <remote>`, `push <remote>`, `fetch <remote>`, `pull <remote>`, `clone <url>` |
| OCI           | `push <oci-remote> <rev> [--runnable <name>] [--json]`, `access token <principal> [--until <duration>]`, `access token --revoke <principal>` |
| records       | `op export`, `op import` (JSONL) |
| copies        | `export <folder>`                                                                              |
| views         | `view [<rev or line>] [--json]`, `view save [<line>] [--json]`, `view show <id> [--json]` |
| overlays      | `overlay create <name> [--readers <principals>] [--mode shared\|follow]`, `overlay list`, `overlay readers`, `overlay use [--add\|--remove\|--none] <names>`, `overlay move [--purge] <name> <paths>`, `overlay log\|show\|diff\|grep\|ls-files <name>` |
| refs          | `ref set <name> <rev> [--expect <rev>] [--ttl <dur>]`, `ref list [<prefix>]`, `ref log <name>`, `ref delete <name> [--expect <rev>]` |
| queue         | `queue add <rev> [--into <line>]`, `queue list [--into <line>] [--json]`, `queue remove <entry>`, `queue run [--into <line>]` |
| changes       | `change list`, `change show <id>`, `change log <id>`, `change diff <id> [--from vN] [--to vM]` |
| setup         | `init`, `projects`, `config`, `cache add|remove|list|repair`, `daemon`                        |

Revisions: `HEAD`, `@`, `HEAD~2`, `HEAD^2`, line names, `line@machine`, tags, `origin/main`,
state id prefixes, git commit ids of imported or pushed states, `stash@{n}`, `HEAD@{n}`
(automatic states, newest first).

Content ids: a state ID is the same only in copies that share its record, but the git ids of its
content are the same on every machine and in git. `layr rev-parse <rev>^{tree}` (and `%T`/`%t` in
`--format`) prints the tree of the commit that `layr git push` exports for the state. `layr rev-parse
<rev>:<path>` prints the id of one path in that commit, as `git rev-parse` does: a tree for a
folder, a blob for a file, the root tree for `<rev>:`. `./` and `../` resolve against the current
folder. A path that the exported commit does not have fails with exit 128.


## Refs, changes, and attestations

Refs name states without a work folder, for example `env/prod` or `pins/build`.
Mutable ref names must start with `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, or `derived/`.
`ref set` refuses line names. Use `commit`, `merge`, or `reset` to change a line.
`--expect` compares the current state before the update. A mismatch changes nothing and returns an error.
`--ttl` accepts positive durations such as `30s`, `5m`, `2h`, and `1d`.
All ref and change commands accept `--json`.

```sh
layr ref set env/prod HEAD
layr ref set env/prod HEAD~1 --expect HEAD
layr rev-parse 'env/prod@{1}'
layr ref list env/ --json
layr ref log env/prod
layr undo
```

`<ref>@{0}` selects the current position. `<ref>@{1}` selects the previous position. `ref log` lists the newest position first.
`ref log` retains positions after delete. `undo` restores a ref if no later update changes it.
Garbage collection (GC) keeps live ref targets and their dependencies.
Expired refs stop resolving. GC records their removal. The normal undo window can retain earlier positions.

These revision prefixes are reserved: `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, `derived/`,
`changes/`, `views/`, and `attestations/`. Line names cannot start with these prefixes.
`views/<id>` resolves a saved view to its shared state. Branch and try also use its overlay stack.

Each new source commit has a `change_id`. Amend, rebase, and cherry-pick preserve it.
A merge commit has a new identifier. Old states without `change_id` remain readable.
`changes/<id>` selects the latest version. `changes/<id>/v1` selects the first version.
`change show` lists versions and their attestations. `change log` lists their operations.
`change diff` compares each version's patch against its own parent, so unrelated base changes do not enter the interdiff.

Attestation kinds: `check`, `approval`, `origin`, `import`, `quarantine`, and `revoke`.
Each attestation is part of an immutable operation record with a machine signature.
It records the exact subject state, optional ChangeId, payload digest, signer, and time.
`--payload` reads a file as the caller and records its SHA-256 digest. It does not store the file contents.
The optional payload reference field is reserved for an object in an allowed layer.

`approve` writes an approval attestation. `approve --withdraw` appends a revoke attestation for the caller's approvals.
`attest revoke` revokes the caller's earlier attestations of the subject on this machine.
The original evidence remains in the log. A revoke cannot withdraw another caller's evidence.
`try` records a check attestation and its command result. Protected-line rules continue to use approvals and checks.
The old `approvals` and `checks` commands fail and name `layr attestations` as their replacement.
Existing signed approval and check records populate the new attestation table without changes to their signatures.
Attestations do not keep a subject alive. Their records remain available while the subject lives.

State kinds `cache` and `derived` have optional `derived_from` and `context` fields.
They have no source parents, line, or ChangeId. Lines, source diffs, merges, and Git exports reject them.
A live ref can keep an auxiliary state and its source alive.
Existing cache snapshots keep their attachment to source states and their wire format for older clients.
They remain cache data outside source diffs. This change does not convert those snapshots into separate StateIds.
The new kinds support separate cache and derived states through the store API.

The database schema version stays at 6. New tables are additive. Optional fields are absent from old state records.


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
File names and symbolic-link targets must use UTF-8.
Symbolic links are archived without access to their targets.

```sh
layr remote add images oci://registry.example/project/app
layr push images main --runnable app --json
```

The remote URL names the image repository.
With one runnable, `--runnable` is optional.
For credentials, set `LAYR_OCI_USERNAME` and `LAYR_OCI_PASSWORD` in the command environment.
Credentials come only from these variables. Project settings are signed records that every machine and store remote receives, so the settings `remote.<name>.username` and `remote.<name>.password` are refused.
A push checks the whole exported tree with the secret detector before any upload.
The client supports Basic and Bearer authentication through the [OCI Distribution API](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md).
HTTPS is the default.
For a local test registry, set `remote.<name>.insecure` to `true` to use HTTP.

A layer uses sorted tar entries, timestamp zero, owner and group zero, and fixed gzip settings.
File modes, link targets, paths, and file bytes determine its identity.
Symbolic-link modes are fixed at `0777` because host systems report different modes for identical links.
The cache uses content hashes.
Compressed layers live in detached Core states with `kind = "derived"`, `context = "oci-layer-v1"`, and `derived_from` set to the source state.
Refs named `derived/oci-layer-v1/<blob-hash>` keep these states through garbage collection.
Identical layer content reuses the same derived state, including across source revisions.
The cache metadata, image configs, and manifests live in `meta/oci`.
A repeated export reuses compressed blobs.
A push checks each blob in the registry before upload.
The default output shows the runnable, destination, digests, and blob counts.
With `--json`, the result includes `state`, `runnable`, `image`, `chart`, `tag`, `uploaded_blobs`, and `skipped_blobs`.

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
layr config registry.user layr-registry
layr config registry.public_url https://layr.example
layr access token alice --until 1d
# Restart the Layr daemon after a registry.listen or registry.user change.
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
The root daemon accepts the connection without reading it.
The HTTP child permanently drops its user, group, and supplementary groups before it reads a request.
It then starts a clean process to remove the daemon's memory and unrelated file descriptors.
`registry.user` selects this user and defaults to `nobody` with its primary group.
The daemon refuses user ID zero or a privilege-drop failure.
Projects on the same listener must use the same user.
Use a dedicated service account.
This account can use only the registry protocol on the service socket, which prevents access through the ordinary CLI protocol.
The executable and the Unix socket's parent folders must allow this user to traverse them.

The HTTP child sends a typed request over the existing service Unix socket.
The root backend checks the socket peer, token, and ACL before it returns manifest or blob bytes.
The backend accepts no file paths or network descriptors.
The HTTP child permits a 4096-byte request line and 16384-byte headers.
It permits one request per connection, with a ten-second read timeout and a sixty-second write timeout.

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

Environment, release, and change tags use the Core revision resolver.
Tag lists include lines, environment refs, release refs, and changes.
An unavailable exact view returns `MANIFEST_UNKNOWN`.
A manifest digest remains readable after the service has generated or exported that manifest.

```sh
skopeo copy --src-creds alice:TOKEN docker://layr.example/project/app:line-main dir:./image
helm pull oci://layr.example/project/charts/app@sha256:DIGEST --username alice --password TOKEN
```

The test file `tests/t30-oci.sh` needs Linux, Btrfs, `docker-registry`, `skopeo`, `helm`, `htpasswd`, `curl`, and Python 3.
It checks external pulls, Basic and Bearer push, service pulls, token revocation, and secret rejection.
It also checks privilege separation, Core tags, and derived-state reuse.

## Ref protection and the merge queue

`protect` accepts ref patterns such as `env/prod` and `release/*`.
Ref rules accept `--update`, `--expect`, `--no-expect`, `--approvals`, `--approvers`, and `--check`.
They refuse `--direct`, `--no-direct`, `--rewrite`, and `--path`, which apply to lines.

A protected ref requires its update role and, by default, `--expect`.
Create a ref before you enable its default protection rule, or use `--no-expect` for its first update.
The new target needs the required approvals from allowed users other than its authors.
A delete requires evidence for the current target. A scoped declassification approval does not count as a subject approval.
Each named check needs a passing attestation for that exact state and the configured command.
`ref set` and `ref delete` run no check commands. Use `try` or the queue to produce the evidence first.

```sh
layr protect env/prod --update maintainer --approvals 1 --check unit
layr ref set env/prod topic --expect env/prod
layr queue add topic --into main
layr queue list --json
layr queue run --into main
```

The queue stores the exact source state and its ChangeId. Later source edits do not change an entry.
The default target is `line.<name>.upstream` when that setting exists, else `main`.
`queue run` processes pending entries in order. An entry becomes `done` or `failed`, with its result or reason.
A failure does not stop later entries. A conflict leaves the target head and files unchanged.
A target head change causes one retry. A second head change fails the entry.
The public `merge --expect` syntax still pins the source. The queue also checks the target head under the merge lock.
The queue uses the existing line rules, approvals, merge drivers, and checks.
A passing check for the exact result state and equal `argv` is reused.
Legacy records without `argv` are reused only for a single word without spaces. Other legacy results run again.
A caller who cannot update a target cannot process its queue.

Queue entries, removals, and results have signed records. Successful line moves and queue completion share one record.
`undo` can restore an entry or reverse a completed move and its status together.
Pending and failed entries hold their source through `holds/queue/<entry>` refs. Remove an entry to release its hold.
All queue commands accept `--json`. Queue list includes pending, done, and failed entries.

Set `queue.run` to a comma-separated list of local line names to enable automatic passes.
A new entry triggers a pass for its configured target. The daemon also runs a pass on each tick.
Automatic passes use the target line owner and the normal line policy. Configured commands run as that owner.
Without `queue.run`, only `queue run` processes entries. Queue runners on one machine use one project lock of their own.


## Workflow

1. A writer makes a line (`layr branch topic-1 main`), or a maintainer makes one for a user
   (`--owner bob`). Only the owner changes a line.
2. The user works and commits in that folder. A program that drives the user can call
   `layr save` at quiet points, and `layr cache repair` when a build tool made a cache folder
   again.
3. Review takes an exact state: `layr diff main...topic-1`, `layr try <state> -- <tests>` runs the
   command in a temporary copy of that state and records the result.
4. A maintainer runs `layr merge topic-1 --expect <state>` in the protected main line: it merges
   exactly the reviewed state and refuses if the line moved. When main's rule names checks
   (`layr config check.unit 'PATH=$HOME/.cargo/bin:$PATH cargo test'; layr protect main --check
   unit`), they run on the merge result as the caller, in a clean environment (the command sets
   what it needs), without the project lock, and main moves only if they pass and
   main did not move in the meantime. A merge never stops on a conflict: the new state lists the
   conflicts, text files get markers, and `layr status` shows them. A line with checks takes no
   result with conflicts: they are resolved in a line first.
5. `layr git push origin <branch>` exports the line's history: each state the remote lacks becomes
   a git commit with the same parents. `--squash [-m <message>]` exports one commit on top of the
   last exported state. The exact commit id is recorded before the push; after a lost response,
   a repeated push reads the remote ref first. With `push.require_check true`, a push needs a
   passing `layr try` of that exact state.

The design and the reasons for it, including what layr leaves to programs built on it, are in
`docs/design.md`.

## Reading and testing other states

A user with the reader role (or a line of their own) reads every state of the project through
the service. A line is not needed: name the project with `--project` (and a line with `--line`
when the command reads a line's head, index or working content).

| Task | Command |
| --- | --- |
| history of a line | `layr --project app --line main log --oneline` |
| one file of a state | `layr --project app show <state>:<path>` (`<state>:<folder>` lists the folder) |
| changes between two states | `layr --project app diff <state1> <state2>`, `diff main...topic-1` |
| files of a line's index | `layr --project app --line main ls-files` |
| search a state | `layr --project app grep <pattern> <state>` |
| git ids of a state | `layr --project app rev-parse <state>^{tree}`, `rev-parse <state>:<path>` |

`layr try <state> -- <command>...` runs a command in a temporary copy of any state the caller may
read, for example tests of another user's line:

```sh
layr --project app try topic-1 -- cargo test          # the head of bob's line topic-1
layr try HEAD~1 -- sh -c 'git log -1 && make check'  # an older state of your own line
```

- The copy is a new folder `lines/.try-<random>` that belongs to the caller. It holds the full
  content of the state, a `.git` folder whose `HEAD` is the state's commit (see "Git in a line"),
  and warm caches by the usual rules. `git status` in it is clean.
- The command runs as the caller, from the root of the copy, with the caller's environment
  except the variables that would point git or layr elsewhere (`GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_COMMON_DIR`, `LAYR_LINE`). `LAYR_TRY_STATE` is
  the state id and `LAYR_PROJECT` the project.
- The caller's line does not change. The copy is deleted afterwards (`--keep` keeps it and prints
  its path); the exit code is the command's, and the result is recorded as a check of the state
  (`layr attestations <state> --kind check`).
- It needs the `try` action (the reader role, or a line of your own), not write access.

## Find the first bad state

```sh
layr bisect run --good HEAD~10 --bad HEAD -- sh -c 'make test'
layr bisect run --auto --good HEAD --json -- sh -c 'make test'
layr bisect run --ref-log env/prod -- sh -c 'make test'
```

Bisect searches the first-parent history between the good state and the bad state.
The command must pass before the fault and fail after the fault.
Each test uses `try` and writes a check attestation.
A check with the same command and state is reused, including a failed check.
The working folder does not change.

Without `--bad`, bisect saves the current working content as an auto state.
Without `--good`, bisect uses the newest ancestor with a passing check for the same command.
Use `--good` when no such check exists.
`--auto` includes the line's auto states between the bounds, in time order.
The result includes the first bad state, its message, its kind, and origin payload digests.
`--json` includes every test step and whether bisect reused its result.
Test output goes to standard error with `--json`.

`--ref-log` searches the positions of that ref.
Its default bad state is the newest ref position.
Its default good state is the newest older position with a passing check for the command.
Explicit bounds must be positions of that ref.

## Secret checks

Layr checks added lines before a shared commit or merge and before an export or push.
It uses exact values from overlays that the caller can read and from files that match `secrets.paths`.

```sh
layr config secrets.paths '[".env", "private/*.key"]'
```

The sources include uncommitted files in the caller's lines.
Candidates include `KEY=VALUE` values and whole files of at most 4 KiB.
Each candidate needs at least 12 characters.
Values with only letters or only digits need at least 20 characters.
The index contains only HMAC-SHA256 values (a keyed hash).
The random key stays in the project's private `meta` folder.
Each check rebuilds the index to include source changes.
For changed files above 64 MiB, Layr checks the whole new file as a stream.
These refusals give a byte offset instead of a line.

A refusal gives the path, line, and a declassify id.
It never prints the value.
An admin can permit that value at that path:

```sh
layr approve --declassify 'src/config.rs@<id-from-refusal>' HEAD
```

This writes an approval attestation with the full hash and path.
The permission applies to that value at that path throughout the project.
A different value or path needs another approval.
`layr approve --withdraw <approval-subject>` revokes the caller's approvals on that subject and restores the check.

Encoded or split secrets are not detected.
This check adds protection to private overlays and does not replace them.

## Git in a line

In a project with a git store (a clone, or a project with a remote), each line has a `.git`
folder for tools that call git. Its objects come from the project store. After every move of the
line's head (commit, amend, merge, reset, undo, pull, push), `HEAD` is the commit of the new head:
the commit that `layr git push` sends for that state, with the commit of the previous state as its
parent, and the index of that state. So `git rev-parse HEAD` changes only when the head moves,
`git status` shows only the uncommitted changes, and `layr rev-parse <commit>` finds the state of
such a commit. The commit is made in the local git store only; nothing leaves the machine before
a push. A push with `--squash` sends another commit for the same state; `HEAD` then shows that
commit. `layr git sync` also records the commits of the head and its ancestors, as a push does.
A `.git` folder that the line owner made instead is left alone.

## Views

A view identifies a shared state and an ordered stack of overlay states.
Its ViewId is the full SHA-256 digest of this canonical JSON, without spaces:

```json
{"format":1,"shared":"<state>","layers":[{"overlay":"<name>","state":"<state>"}]}
```

Field order follows the example. Layer order follows the stack.
Access policy stays outside the identity. Changes to reader rights do not change the ViewId.
A state without overlays has an empty `layers` array.
Working edits do not change the descriptor until a state changes.

```sh
layr view main --json
layr view save main --json
layr view show <id> --json
layr branch reproduce views/<id>
layr try views/<id> -- ./test.sh
layr attest approval views/<id>
layr attestations views/<id> --kind check --json
```

`view save` writes a signed `view.save` record. Repeated saves of the same descriptor write no extra record.
`views/<id>` resolves after that record reaches the local machine.
Commands that need a state use the shared state. Branch and try use the complete stack.
`try main` and `try HEAD` also use the line's current stack.
An explicit state revision has no layers.

A branch from a view preserves its exact layer states, including older states of shared-mode overlays.
It initializes missing overlay lines. It does not move an existing overlay line.
A later private commit can fail the existing stack check if that overlay line has advanced.
Use `layr overlay use` to take its newest state before the commit.

The caller must read every layer before branch or try can materialize a view.
A different branch owner must also read every layer. A refusal names the overlay.
Descriptors contain overlay names and state ids. Project readers may read descriptors without access to the private files.
A descriptor can travel to another machine while its private layers stay local.

Try checks with layers name the view as their attestation subject.
Checks without layers retain the state subject. Check payloads include the exact argument array.
`LAYR_TRY_STATE` names the shared state. `LAYR_TRY_VIEW` also names the view when it has layers.
A check of one view does not certify a different stack with the same shared state.

Saved descriptors and attestations do not keep states from garbage collection.
`view show` can still print a descriptor after a state is collected.
Full-view resolution reports `view <id> refers to a collected state <state>`.
Before collection, pin the shared state with `layr ref set pins/<name> views/<id>`.
Overlay states stay through their own live line history.

## Merge engine

Change lists of both sides come from `btrfs send --no-data` against the common base. A path
changed on one side takes that side (a reflink copy). Tree rules cover moves (inode level),
folder deletes against new files, links, executable bits, file/folder type changes, add/add,
rename/rename and delete/modify. Files changed on both sides use the driver from
`.gitattributes` or `.layrattributes`:

| driver            | behavior                                                                          |
| ----------------- | --------------------------------------------------------------------------------- |
| `text` (default)  | three-way line merge (`git merge-file`)                                           |
| `union`, `ours`, `theirs`, `binary` | as in git                                                       |
| `blocks`          | binary merge by byte ranges from the change lists: allowed when both sides kept the inode and size and the ranges do not overlap; `merge-verify=<name>` runs a check command |
| `<name>`          | `merge.driver.<name>` command with `%O %A %B %P`, run as the caller of the merge     |

### Derived files

Declare outputs in `.layrattributes` or `.gitattributes`:

```text
package-lock.json derive=npm
```

Set commands in the project config, outside the versioned tree:

```sh
layr config derive.command.npm 'npm install --package-lock-only'
layr config derive.inputs.npm 'package.json packages/*/package.json'
```

- Derive runs when a merge makes a new tree, including cherry-pick and rebase.
- A fast-forward keeps the exact target state, including its derived files.
- A changed input or output on either side starts the rule.
- Inputs use whitespace-separated path patterns with Git ignore syntax.
- Derived outputs bypass the file merge. Literal output paths may be absent before the merge.
- Rules run by name in a stable order. Dependencies run first when their outputs match another rule's inputs.
- Dependency cycles become conflicts. A failed command keeps the ours output and adds a conflict to the state.
- Commands run as the caller in a private copy with a clean environment, as command drivers do.
- Only declared regular output files return to the result. Commands cannot return symbolic links.
- Commands have the caller's network access. Layr does not isolate the network or manage registry caches.
- Pin tool and dependency versions. Use commands that produce the same output from the same inputs.
- Layr does not run commands twice to check determinism.

### Batch lines and hard reset

```sh
layr branch trial HEAD --count 10 --json
layr --project app --line trial-1 reset --hard main
```

The branch command creates `trial-1` through `trial-10` after validation of all names.
Each line has one ordinary `line.create` record. Existing log readers need no format change.
If a later line fails, completed lines stay valid. The error names those lines.
The command removes incomplete line folders and temporary snapshots.

On btrfs, batch creation submits standard snapshots with at most eight workers.
The kernel can share transaction commits between concurrent requests.
The supported ioctl API does not guarantee one transaction for all snapshots.

Measured on 2026-10-22 UTC in OrbStack `layr-dev`, Linux `7.2.9-orbstack-00391-g2b298d05fbd4`.
The debug binary runs as root without the service on a fresh btrfs loop filesystem with 100 source files.
The table shows medians of three trials. The order alternates between batch and single commands.

| Lines | Separate branch commands | One `branch --count` command | Less time |
| ---: | ---: | ---: | ---: |
| 10 | 1.585 s | 1.192 s | 24.8% |
| 100 | 19.324 s | 6.572 s | 66.0% |

The result supports retaining eight concurrent snapshot workers.
These times include command startup and database writes. They do not isolate the transaction cost.
Individual trials vary. The result does not establish a constant time for any batch size.
Run `sudo env LAYR=$PWD/target/debug/layr bash benchmarks/branch-batch.sh` to repeat the test.
See the [raw trials](benchmarks/results/cloud-branch-batch.jsonl).

Hard reset can replace an idle btrfs line with a snapshot of the target state.
The copy path preserves local files, caches, overlays, and active process references when a swap is unsafe.
A call from inside the line also uses the copy path to preserve the caller's current directory.
The snapshot path keeps the existing `.git` subvolume. An ordinary `.git` directory uses the copy path.

### Conflict radar

`layr status` lists path overlaps with other live lines of the same project.
`layr status --json` returns an `overlaps` array with `path`, `line`, `owner`, and `changed_at`.
`changed_at` uses Unix milliseconds from the other file, or its state for a deletion.

Radar compares paths against each pair's common base with the existing change lists.
Each readable overlay uses its own common base.
This line uses its current content. Other lines use their newest saved state, or their head when newer.
Unsaved changes in other lines appear after `layr save`.
Project read access and overlay reader rules apply. Unreadable private paths stay hidden.
Short and porcelain status output keeps its existing format.
Radar writes no permanent records. A later event API can expose these results.

## Storage and consistency

```text
$LAYR_ROOT/machine/{id,name,key}       machine identity, Ed25519 key (0700)
$LAYR_ROOT/projects/<p>/
  states/<state-id>                    read-only snapshots (0700)
  lines/<line>                         working folders (0711; each line 0700, its owner)
  git/                                 git object store for the remote bridge
  meta/db/layr.sqlite                  signed records + derived tables (own subvolume, 0700)
  meta/stage, meta/scan, meta/work     staging lines and temporary snapshots
  meta/base/<line>@<key>               full tree of a line with overlays (derived)
  meta/overlays/<overlay>/             an overlay: the internal project <p>%<overlay>, same layout
```

An operation writes its snapshots first, then one SQLite transaction (synchronous=FULL) with
its record and the derived rows. After a crash only a snapshot without a record can exist;
`layr fsck --repair` deletes it. Derived rows keep the order key of their record, so a rebuild
(`layr reindex`) gives the same result as incremental imports.

## Security model

- The service runs as root; every other user is a client (`SO_PEERCRED`). Clients cannot read
  `states/` or the database; they read history through layr.
- The socket (`/run/layr/layr.sock`, mode 0666 in a folder of mode 0755) is open to every local
  user: the service decides each request by the caller's uid, groups and role, so a reader runs
  every read command. A connection refused with "Operation not permitted" (EPERM) never comes
  from layr: a sandbox or seccomp filter of the calling process refused the `connect()` system
  call (for example a filter that denies `connect` for every socket family when network access is
  off). Such a process needs `connect()` to the socket allowed, or must run layr outside that
  sandbox; the client error says so.
- The git object store (`git/`) holds history too. Its folder alone decides who reads it: open
  to the users and groups with a role and the owners of lines (an access list, or `0755` when
  everyone may read). git writes below it with `core.sharedRepository=0644`, so a strict umask
  of the service (such as `0077`) does not close the objects that the `.git` folder of a line
  uses through `alternates`. A store of an earlier layr gets read access below its folder once:
  at the next new line or role change, or at the first periodic run after the service starts.
- A line belongs to its owner. All writes of the service into a line go through descriptors
  (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`); a link in a line never redirects a
  root write. New working folders are prepared in a private folder and appear when ready.
- Commands for users (git network access, `try`, checks, `sync`, merge drivers, `apply` in a
  line) run with that user's uid, groups and a clean environment: a command never runs as
  another user. Paths named by a user outside the store (export, remote copies, patch files) are accessed with the user's file permissions, and so
  is a file such a command leaves for the service (the result of a merge driver).
- Overlays (docs/overlays.md): an overlay is read only through its own project, by its readers;
  the working content of a line with overlays is open to its owner and to readers of all its
  overlays; automatic states leave overlay files out, so no state of the project holds one.
- Permissions (docs/design.md, section 3): roles per project and machine (`layr access`), deny
  rules that win over any role, owners of lines, and protection rules of line patterns
  (`layr protect`). A new project has one admin (its creator) and a protected `main`. Stores of
  older versions keep their `admin` (or `lead`) and `members` settings as roles until `access`
  is set. Root may do everything, and every record names who acted.
- Replication takes records only from trusted machines (`layr sync` trusts the peer the admin
  chose once its records passed the checks; `layr remote trust <name> <machine>`), checks the project id,
  signatures and sequence numbers, and refuses a changed copy of a known record. Roles, deny
  and protection rules, merge commands and remotes are per machine; other
  machines' records never change a local line.
- A btrfs stream from a peer passes a check on its way to `btrfs receive`, which
  would otherwise find snapshot and clone sources by UUID anywhere on the file system: one
  subvolume with the name and UUID its signed record names, a parent only from the same
  project, and clone sources limited to that parent, the new subvolume, or explicitly verified states of the same project.
  Extra sources require an exact stream UUID and transaction ID. Their file paths cannot traverse symbolic links. After the receive, the new subvolume must be a snapshot of exactly that parent.
- Files larger than 64 MiB count as binary for diff, grep, blame, rename detection and merge:
  the service never reads such a file into memory (`show` and merge drivers copy it in
  pieces). Patches, messages and records named by a user are limited in size too.
- The service limits running requests per user and in total, and drops a client that does not
  send its request within 10 seconds.

## Remotes and replication

Add a peer with `layr remote add work ssh://host/project`.
Run `layr sync work` to exchange signed records and missing states in both directions.
Layr starts `ssh host layr serve-peer project`.
The internal peer command does not appear in help.
Use `--auto --caches` to include automatic states and saved caches.
The final snapshot of a deleted line always travels, because `undo` needs it.

- `layr push work` sends records and states to the peer.
- `layr fetch work` receives records and states from the peer.
- `layr pull work` fetches, then merges the newest head of the current line from another machine.
  The merge can fast-forward. Conflicts remain in the new state.
- `layr remote list` shows remotes and known machines.
- `layr remote trust work <machine>` trusts a known peer key.
  An unknown key requires `--public-key <ed25519>`.
  A new host gets its identity with `layr remote self --json`, outside any project.
  The result contains `id` (the machine UUID) and `public_key` (64 hexadecimal characters).
  For a store remote, trust records the machine key and adds its encrypted machine slot.
- `layr remote remove work` removes the remote configuration.

Remote configuration uses signed `config` records in the project database.
Layr remotes use `cloud.remote.<name>.url`. Git remotes keep `remote.<name>.url`.
Configuration remains local to each machine, as other project settings do.
A peer cannot replace the local transport URL through sync.
The list command supports `--json`.

Create a new store with `layr remote add archive file:///path --paper-key-out /private/path/key.txt`.
The output file must not exist. It must be outside the store and all line folders.
Layr creates the file as the caller with mode `0600`.
Write the words on paper. Delete the file after you check the paper copy.
Layr never prints the words or puts them in project records or configuration.
S3 credentials come only from the caller's environment in `Ctx.env`.
Layr does not store these credentials in the project database or local index.

Peer sync does not delete state snapshots. A local garbage collection does not delete snapshots on another machine.
The operation log keeps earlier line positions for `layr undo <operation>` within the local retention window.

The URL scheme selects the remote type:

| URL | Type | Operations |
| --- | --- | --- |
| `ssh://host/project` | Peer | `sync`, `push`, `fetch`, `pull` |
| `file:///path`, `s3://bucket/prefix` | Store | `push`, `fetch`, `pull`, `clone`, `remote trust` |
| `oci://host/project` | Registry | `push <remote> <rev> --runnable <name>` |

The store and registry hooks return explicit errors until their workstreams are integrated.
Store remotes require no Layr process at the destination.

Use `layr git remote`, `layr git fetch`, `layr git pull`, `layr git push`, `layr git clone`, and `layr git sync` for Git.
The previous Git forms fail with the new command name.
The previous command transport form fails with `layr sync <remote>` in the message.
Tests can use `layr sync work --command -- <command> [<arguments>]` with an explicit peer remote.

## Btrfs execution and sync

Metadata comparisons call the kernel directly. They do not start a `btrfs send` process.
Comparisons of immutable state pairs can reuse a private cache in `meta/diff-cache`.
The cache holds at most 256 entries and 32 MiB of file data per project.
Each entry contains at most 8 MiB, including its checksum.
New snapshots and temporary scans do not enter this cache during the command that creates them.
A missing or damaged entry causes a fresh kernel comparison. Cache writes do not require an fsync.
The cache is disposable and is not part of records or remote copies.

Sync peers exchange capability fields in their initial message:

- Peers that support extra clone sources can reuse data from up to eight additional shared states.
  The sender examines at most 32 recent candidates for each state.
  Peers exchange source identities and use ordinary data transfer when transaction identities do not match.
  The receiver checks each source against a signed local state, its UUID and its read-only flag.
  Received UUIDs must also match their recorded send transaction; original local UUIDs identify the source directly.
  Reuse of original local UUIDs requires btrfs-progs 6.11 or later. Older receivers get file data for those sources.
- Peers with compatible btrfs tools can exchange compressed extents with stream protocol 2.
  The sender also checks kernel support. This preserves existing compression; it does not compress uncompressed files.
- A peer without these capabilities gets the previous stream and clone rules.
  Remote copies retain protocol 1 and the previous manifest format.

Working content still requires an atomic snapshot before comparison.
The optimization does not replace this snapshot with an unsafe check of a writable tree.
It does not enable filesystem compression or run defragmentation automatically.
Defragmentation can break shared extents and increase disk use, as described in the
[btrfs documentation](https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html#subcommand).
Protocol requirements are described in [btrfs-send](https://btrfs.readthedocs.io/en/latest/btrfs-send.html).

Reproduce the measurements on Linux with root access:

```sh
python3 benchmarks/btrfs-optimizations.py --before /path/to/old/layr --after target/release/layr \
  --root /new/path/on/btrfs --files 10000 --repeats 12
bash benchmarks/sync-storage.sh /path/to/old/layr target/release/layr /tmp/sync-results.json
OLD_LAYR=/path/to/old/layr LAYR=$PWD/target/release/layr bash tests/compat-btrfs.sh
```

The compatibility test covers state IDs, signatures and both sync roles with an older binary.
It also checks saved cache content across old and new clients.
The CLI uses `cache` and `--caches`; `layer` and `--layers` remain aliases.
Cache records retain the legacy `save.layers` name and `layers` fields on disk.
The reader also accepts the intermediate `save.caches` format.
Sync selects the cache message name from the peer capabilities.

### Measured btrfs optimization results

Measured on 2026-10-14 in the OrbStack Linux VM, with release binaries and 10,000 files of 4 KiB.
The final test uses a separate 8 GiB loop filesystem and 12 pairs of commands with alternate execution order.
The table shows median elapsed time, including command startup. The immutable diff uses a warm cache.

| Operation | Before | After |
| --- | ---: | ---: |
| Clean status | 21.36 ms | 14.42 ms |
| Repeated diff between states | 8.33 ms | 3.63 ms |
| Clean save | 18.98 ms | 14.46 ms |

The first diff with a cache miss takes 10.12 ms in this run.
Earlier tests on the shared filesystem sometimes show slower status and save commands after the change.
These results do not establish a general elapsed-time improvement for those commands.
Repeated immutable diffs improve in every recorded run. Unrelated host activity was not controlled.

Separate sync tests check exact file content and allocated extents after a filesystem sync:

| Fixture | Stream bytes before | Stream bytes after | Reduction |
| --- | ---: | ---: | ---: |
| A 16 MiB file removed, then restored through a reflink | 33,579,189 | 16,783,017 | 50.0% |
| An 18,432,000-byte text file already compressed with zstd | 18,449,311 | 590,689 | 96.8% |

The restored file shares all 16 MiB of its data extents after sync. Previously, it had a separate 16 MiB allocation.
This space comparison excludes filesystem metadata. Compression savings depend on the source data and its existing compression.

Raw results contain binary hashes and individual samples:
[final timing test](benchmarks/results/btrfs-opt-isolated-10000.json),
[sync and extent checks](benchmarks/results/btrfs-opt-sync.json),
[initial timing test](benchmarks/results/btrfs-opt-initial-10000.json),
[shared filesystem repeat](benchmarks/results/btrfs-opt-shared-repeat-10000.json),
and [shared filesystem CPU measurements](benchmarks/results/btrfs-opt-shared-cpu-10000.json).
The earlier runs use intermediate builds; their hashes identify each build.

## Performance

One run in a Linux VM (OrbStack, 17 vCPU, kernel 7.0), release build, btrfs on a loop file.
Project: 200,000 small files and one 100 MiB file. The second column follows a write of 300 MiB
of ignored build output, so btrfs is still flushing; the third is a quiet tree.

| Operation                                | after a large write | quiet  |
| ---------------------------------------- | ------------------: | -----: |
| `status`, clean                          | 0.02 s              | 0.02 s |
| `status`, 101 edits                      | 0.80 s              | 0.05 s |
| `diff --stat`                            | 0.24 s              |        |
| `add -A`                                 | 0.68 s              |        |
| `commit`                                 | 1.48 s              | 0.35 to 0.5 s |
| `branch` (a new line, any size)          | 0.18 s              | 0.28 s |
| `merge` (50 + 50 changed files)          | 1.35 s              |        |
| `log -5`, `show --stat`                  | 0.003 s, 0.014 s    |        |
| `git status` of the same tree, for scale | 0.14 s              |        |

Most of the time is btrfs transaction commits: every snapshot commits one (15 to 600 ms, more
while dirty data is being written). The database lives in its own subvolume: an fsync there
after a snapshot takes about 6 ms, against about 100 ms in the subvolume that holds the
snapshot folders.

### Experimental APFS comparison (provisional)

Measured on 2026-10-14 with the implementation in `ada0cef`.
Each fixture contains 1,000 or 10,000 files of 4 KiB. Ten files receive an extra line.
The source content hashes match across both backends. Each operation has three trials.
The table gives median seconds, including command startup.

APFS runs natively on macOS 27.2, on a case-sensitive volume in an ASIF image.
btrfs runs in the OrbStack Linux VM on the same Mac.
Those initial backend runs were sequential. The host also ran other tasks. Their load was not controlled.

| Operation | btrfs, 1,000 files | APFS, 1,000 files | btrfs, 10,000 files | APFS, 10,000 files |
| --- | ---: | ---: | ---: | ---: |
| init | 0.354 | 1.040 | 0.509 | 18.289 |
| branch | 0.035 | 0.285 | 0.032 | 3.618 |
| status, clean | 0.016 | 0.674 | 0.014 | 7.758 |
| diff, 10 edits | 0.021 | 0.627 | 0.019 | 7.766 |
| add -A | 0.028 | 0.897 | 0.024 | 12.314 |
| commit | 0.030 | 0.534 | 0.025 | 8.849 |
| merge, fast-forward | 0.043 | 1.350 | 0.040 | 35.101 |

These values are provisional. They do not establish an isolated filesystem speed ratio.
A later test alternates clean status commands on both backends with 10,000 files.
It measures APFS at 11.00 to 13.26 seconds and btrfs at 0.09 to 16.33 seconds.
The host's one-minute load average ranges from 294 to 311 during that test.
A further benchmark found no quiet interval within 30 minutes. The comparison remains provisional.

The measured baseline clones every file for a snapshot and scans content for changed blocks.
The btrfs implementation uses subvolume snapshots and send-stream change lists.
Those mechanisms have different costs as the file count grows.
The test image still has 96% free space after the runs. Spotlight indexing is disabled on it.

Raw data includes the binary hash and source content hash:
[APFS, 1,000 files](benchmarks/results/apfs-1000.json),
[APFS, 10,000 files](benchmarks/results/apfs-10000.json),
[btrfs, 1,000 files](benchmarks/results/btrfs-1000.json),
[btrfs, 10,000 files](benchmarks/results/btrfs-10000.json),
and [interleaved status](benchmarks/results/paired-status-10000.json).

At the benchmark revision, the previous btrfs binary (`c6063e4`) and the candidate passed thirteen compatibility checks.
Both binaries read each other's states and verify signatures without changing state identifiers.
Run those checks with `OLD_LAYR=/path/to/old/layr LAYR=/path/to/new/layr bash tests/compat-btrfs.sh` on Linux as root.

A later APFS run includes overlays and the compatible transfer implementation.
Three trials give these median times:

| Operation | 1,000 files | 10,000 files |
| --- | ---: | ---: |
| branch | 0.388 s | 4.058 s |
| status, clean | 1.074 s | 9.264 s |
| commit | 0.850 s | 8.666 s |
| merge, fast-forward | 2.401 s | 22.721 s |

The host had other work, so these results remain provisional.
The Linux comparison could not start because the OrbStack executable was absent.
These results do not establish a speed ratio against btrfs.
The measured binary predates the final service permission checks.
Raw data: [1,000 files](benchmarks/results/apfs-complete-1000.json)
and [10,000 files](benchmarks/results/apfs-complete-10000.json).

## Not in this version

- APFS volume snapshots and incremental APFS sends. Current snapshots use verified directory clones; APFS sends use full streams.
- Overlays beyond the first slice: routing patterns, ephemeral mode, folder overlays, encrypted
  remote copies and replication of overlays (`docs/overlays.md`, section 9.7).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Cache snapshots are checked the same way through the signed `save.caches` record.

### APFS service test

The service uses kernel peer credentials and runs each request in a separate process.
The client supplies file descriptors for its current folder and standard streams.
The service changes credentials before it accesses caller-owned paths or starts caller commands.

Enable ownership on the dedicated test volume before the service test:

```sh
sudo diskutil enableOwnership /Volumes/YourCaseSensitiveVolume
sudo env LAYR_APFS_TEST_ROOT=/Volumes/YourCaseSensitiveVolume \
  LAYR_APFS_TEST_USER="$(whoami)" LAYR="$PWD/target/debug/layr" \
  bash tests/apfs-service.sh
```

The test uses the current user and `nobody`. It creates no system accounts.
Use `tests/apfs-cross.py` to test peer sync compatibility with a Linux binary in an OrbStack VM.

### Encrypted store remotes

A `file://` or `s3://` remote keeps signed records and encrypted state files.
It can restore a project after the loss of every machine.
The [store format](docs/remote-format.md) describes the objects and key slots.

```sh
layr remote add archive file:///srv/layr/project --paper-key-out /secure/project-paper.txt
layr push archive
layr fsck --remote archive --files
layr log --remote archive --json
layr gc --remote archive --dry-run
```

The paper file must be outside the stores and line folders. A new file has mode 0600.
The command prints its path. It never prints the words.
Copy the words to paper. Check the copy. Delete the file.

A machine slot uses the existing machine Ed25519 key. A machine without a slot can push but cannot fetch.
Use `layr remote trust <remote> <machine>` from a machine with a slot to grant read access.
For a new machine, get its `layr remote self --json` output. Give its key with `--public-key <key>` when you grant access.
A machine that retains the vault identity can read future objects. Full revocation requires a new vault identity.

S3 reads the caller's `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, and `AWS_REGION` environment variables.
For MinIO, set `AWS_ENDPOINT_URL_S3`. Credentials stay out of the project records and local index.
Writes use conditional S3 puts. Large objects use multipart upload.
The part target is 16 MiB. A larger frame has a separate part, same as Desk v1.

The store keeps complete states and records. Remote garbage collection removes only incomplete data attempts older than 24 hours.
A retry uses a new state attempt. Record retries preserve the signed bytes.
If a push stops before a line head is complete, clone reports the missing line and returns status 2.
The project keeps its available lines. Run `layr fetch origin` later to create the missing working folders.
Private overlay files stay local. Legacy cache layer snapshots require a peer remote.
The `--caches` option sends states of kind `cache`.

To import a Desk v1 vault, set `LAYR_PAPER_KEY_FILE` to the Desk paper file. Then use `layr clone <vault URL> <project>`.
The import creates line `main`, preserves history order, and verifies each file's SHA-256.
The source vault stays read only. Add a new v2 remote to save the imported history.
