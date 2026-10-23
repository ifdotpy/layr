# Layr

**Version control built from folder snapshots.** Every line of work is a real folder. Every saved
state is a frozen, read-only copy of that folder, made by the file system in constant time. Every
operation is a signed record that you can undo.

Layr is for projects where many people and many AI agents work on one machine at the same time:
dozens of parallel working folders with warm build caches, exact review of what will ship, and a
history that no tool, script or agent can silently lose. Git stays available as a mirror for the
outside world (`layr git push`).

```sh
layr line new agent main --count 16       # 16 working folders with warm caches, in seconds
layr save -m "Add discount field"         # freeze the folder as a state
layr queue add agent-3 --into main        # merge only after checks pass on the exact result
layr ref set env/prod head --expect env/prod   # ship exactly what was checked
docker pull layr.example:5080/shop/app:prod     # and every ref is a container image
```

## The model in one minute

Git stores files and computes folders. Layr stores folders. Five ideas cover everything else:

```text
   line  main/           a real folder you work in (writable btrfs snapshot)
     │  save
     ▼
   state 7d3e            a frozen copy of that folder (read-only snapshot + signed record)
     ▲         ▲
     │         └── attestation   a signed note about a state: "tests passed", "approved", "origin"
     └──────────── ref           a name for a state: env/prod, release/1.4.2, pins/demo

   remote                another place with the same states: a machine, a folder, S3, Layr Cloud
```

| Idea | What it is | Git analogy |
|---|---|---|
| **line** | A writable folder at `projects/<p>/lines/<name>`, with an owner and a head state | branch and worktree in one |
| **state** | A read-only copy of a folder, with a signed record (author, parents, message) | commit |
| **ref** | A name that points at a state, moved only with compare-and-swap (`--expect`) | tag that can move safely |
| **attestation** | A signed statement about one exact state: a check result, an approval, an origin | CI status, but signed and kept |
| **remote** | Another copy: a peer machine (`ssh://`), a folder (`file://`), S3 (`s3://`), Layr Cloud (`layr+https://`) | remote |

Three consequences follow from "folders, not files":

- **Branching is free.** A new line is a snapshot: constant time, no copy, build caches included.
- **Nothing is lost.** Layr saves an auto state before every changing command, on `layr save`,
  and every ten minutes. It sees what any program did to the folder, not only what an editor
  reported.
- **Review means the exact folder.** A check, an approval and a deploy all name one state id.
  If anything in the folder changes, the id changes, and the approval no longer applies.

## What it looks like

### Undo anything, including what a shell script did

An agent ran `rm -rf src/` through a shell. Tools that record only their own edits miss this.
Layr records the folder.

```console
$ rm -rf src tests                 # an agent, through a shell
$ layr op log -n 3
2f72827b87c3 2026-10-23 21:03:20 +0000 agent-7 save 9d55834
6027335d14ee 2026-10-23 21:03:20 +0000 agent-7 save 185b9dc
7796731c8a2d 2026-10-23 21:03:18 +0000 agent-7 save 5356156 Add discount field
$ layr diff 185b9dc 9d55834 --stat | tail -1
 44 files changed, 46 deletions(-)
$ layr restore --source 185b9dc .
```

A program that drives agents calls `layr save` after each step, so each step is one state.

### Run many agents in parallel, with warm caches

```console
$ layr line new agent main --count 16
Created line agent-1 at /var/lib/layr/projects/shop/lines/agent-1 from 5356156
…
Created line agent-16 at /var/lib/layr/projects/shop/lines/agent-16 from 5356156
$ cd ../agent-3 && cargo build          # target/ is already warm: a copy-on-write clone
```

Measured: 16 lines with private 128 MiB caches use **153 MiB** in Layr and **2508 MiB** with Git
worktrees and full cache copies ([method](docs/performance.md)). Creating 100 lines at once is
66% faster than 100 single creations.

### Merge without fighting lockfiles

A derived file is rebuilt from the merged tree instead of merged line by line:

```console
$ cat .layrattributes
package-lock.json derive=npm
$ layr config derive.command.npm 'npm install --package-lock-only'
$ layr config derive.inputs.npm package.json
$ layr merge feature-a
Derived npm
Merge made by the 'layr' strategy: c13cbb6
 package-lock.json | 2 +-
 package.json      | 2 +-
 2 files changed, 2 insertions(+), 2 deletions(-)
```

Both sides changed `package.json`; Layr merged it and then ran the command on the merged tree, so
the lockfile matches the merged dependencies.

### Find the step that broke the build

```console
$ layr bisect run --auto --good v1.4 -- cargo test -q
01a16a93-e270-727e-a20a-c13cbb69f6a2 good
01a16a93-e34b-74fa-b833-f50d15a7011c bad
01a16a93-e2f1-76da-a7db-816b62e715eb good
01a16a93-e313-71c7-b98b-77241efbd518 bad
First bad state: 01a16a93-e313-71c7-b98b-77241efbd518 (auto)
```

`--auto` searches the auto states between commits, so the answer is one step, not one commit.
Results are kept as attestations, and a repeated bisect reuses them.

### Review behavior, not only lines

Probes are commands in `layr.toml` that write outputs. Layr stores their results per exact state
and compares them:

```toml
[probe.size]
run = ["sh", "tools/probe.sh"]
outputs = ["out/metrics.json"]
kind = "number"            # also "files" (text diffs) and "json" (structural diffs)
```

```console
$ layr probe run main
size: ran exit 0 (outputs 01a16a93-e563-77e7-be62-bb7b58c2bb7d)
$ layr probe run changes/01a16a93-e524
size: ran exit 0 (outputs 01a16a93-e58c-7594-8ba3-64ee948e34dc)
$ layr change show 01a16a93-e524
change 01a16a93-e524-778c-af19-094611d98658
v1	01a16a93-e51e-7477-89d6-aa7d60394c80	Grow notes
  check	01a16a93-e590-75ec-8792-71ad60937307
Behavior
  size: changed
    out/metrics.json   lines 23 → 29 (+26%)
```

`change show` and `probe diff` never run anything; only an explicit `layr probe run` does.

### Ship exactly what was checked

```console
$ layr config check.unit 'cargo test'
$ layr protect main --check unit
$ layr protect 'env/*' --update maintainer --check unit
$ layr queue add topic --into main
$ layr queue run --into main
$ layr ref set env/prod main --expect env/prod
fatal: ref 'env/prod' needs a passing check 'unit' for exactly state 816b62e and command 'cargo test'; run layr try first
$ layr try main -- cargo test && layr ref set env/prod main --expect env/prod
env/prod	01a16a93-e2f1-76da-a7db-816b62e715eb
$ layr ref set env/prod 'env/prod@{1}' --expect env/prod      # roll back
```

A protected ref moves only to a state that has the required approval and a passing check
attestation for that exact state. The merge queue runs the checks on the exact merge result and
moves `main` only if they pass.

### Every ref is a container image

```console
$ layr config registry.listen 127.0.0.1:5080
$ docker pull 127.0.0.1:5080/shop/app:prod          # env/prod
$ docker pull 127.0.0.1:5080/shop/app:change-812    # a preview of change 812
```

Images are deterministic: the same state gives the same digest on any machine. A Helm chart from
the same state gets the exact image digest. [OCI guide](docs/oci.md)

### Keep secrets out of shared history

Layr knows the actual values in private overlays and `.env`-style files, and refuses any commit,
push or export that contains one. It never prints the value.

```console
$ layr save -am "Add Stripe client"
fatal: secret detected at pay.rs:1; declassify id pay.rs@d35e9d18fe00e769; admin: layr approve --declassify 'pay.rs@d35e9d18fe00e769' 01a16a93-4f62
```

### Encrypted copies anywhere

```console
$ layr remote add vault s3://my-bucket/shop --paper-key-out ~/paper-key.txt
paper key file: /home/alice/paper-key.txt
$ layr push vault
push: 34 records, 10 states, 4388 bytes
$ layr clone s3://my-bucket/shop                  # on a new machine, with its machine slot
$ layr mount vault                                # read-only disk at ~/Layr, decrypted locally
```

Data is encrypted with `age` before it leaves the machine. Storage, gateway and cloud see
ciphertext. [Remotes guide](docs/remotes.md)

## Install and first steps

Requirements: Linux with btrfs for the store (a loop file is enough), `btrfs-progs`, `git`, `zstd`.
The service runs as root, because snapshots into protected folders and `btrfs send` need it;
every user talks to it through a Unix socket.

```sh
cargo build --release
sudo target/release/layr daemon &                      # the service, /run/layr/layr.sock
sudo layr init app --from ~/src/app --owner alice      # alice is admin; main is protected
layr access grant bob writer                           # (alice) roles: reader, writer, maintainer, admin
layr line new topic main                               # (bob) a new line: its own folder
cd /var/lib/layr/projects/app/lines/topic
layr status; layr diff; layr add -A; layr save -m "Change"
layr merge topic --expect <state alice reviewed>       # (alice, in main) exactly that state
```

The store lives under `$LAYR_ROOT` (default `/var/lib/layr`). To work from another path, link
the line folder.

## Commands

| Group | Commands |
|---|---|
| work | `status`, `diff`, `add`, `rm`, `mv`, `apply`, `save [-m <message>] [-a] [--amend]`, `restore`, `reset`, `park`, `clean` |
| history | `log`, `show`, `blame`, `grep`, `ls-files`, `rev-parse`, `merge-base`, `undo`, `op log` |
| lines | `line new\|list\|delete\|rename`, `merge [--expect]`, `pick`, `invert`, `replay`, `adopt`, `tag` |
| review | `change list\|show\|log\|diff`, `attest`, `attestations`, `approve`, `try <rev> -- <cmd>`, `probe run\|diff`, `bisect run` |
| delivery | `ref set\|list\|log\|delete`, `protect`, `queue add\|list\|remove\|run`, `view`, `push <oci-remote>` |
| remotes | `remote add\|list\|self\|remove\|trust`, `push`, `fetch`, `pull`, `sync`, `clone`, `mount` |
| Git bridge | `git remote`, `git fetch`, `git pull`, `git push [--squash]`, `git clone`, `git sync` |
| setup | `init`, `projects`, `config`, `access`, `cache`, `overlay`, `gc`, `fsck`, `reindex`, `daemon` |

Revisions: `head`, `head~2`, `head^2`, line names, `line@machine`, state id prefixes, tags,
refs such as `env/prod` and `env/prod@{1}`, `changes/<id>[/vN]`, `views/<id>`,
`attestations/<id>`, `park@{n}`. [Full command reference](docs/commands.md)

Output of `diff`, `status --porcelain`, `show` and `log --format` matches Git byte for byte for the
same content, so tools that parse Git output keep working.

## How it works

- **Snapshots, not copies.** A line is a btrfs subvolume; a state is a read-only snapshot of it.
  Change lists come from btrfs metadata (`send --no-data`) in O(changes), and merges copy files
  with reflinks.
- **One gate.** A root service performs every operation and checks the caller (`SO_PEERCRED`).
  History, other users' lines and the database are readable only through it.
- **Signed records.** Each operation is an Ed25519-signed record of the machine that made it.
  Machines exchange records and states; a record from another machine never deletes local states.
- **Derived data stays outside history.** Build caches, OCI layers and probe outputs are states of
  kind `cache` or `derived`. They never enter a line's history, diffs or Git exports.

The design and its reasons are in [docs/design.md](docs/design.md).

## Platforms

| Backend | Status |
|---|---|
| btrfs on Linux (kernel 5.6 or newer) | supported |
| APFS on macOS | experimental, see [docs/apfs.md](docs/apfs.md) |
| OverlayFS on ext4 or XFS | experimental, see [docs/overlayfs-backend.md](docs/overlayfs-backend.md) |

Unsafe code is limited to `src/sys.rs` (ioctls, fork, user switching); everything else uses
`rustix` and the standard library.

## Layr Cloud

[Layr Cloud](https://github.com/ifdotpy/layr-cloud) runs Layr in your own AWS account: an S3
store per project, a gateway for machines without AWS credentials (`layr+https://`), and an
executor host that runs the merge queue and the registry. It replaces Layr Desk. This repository
holds the client side: the store format ([docs/remote-format.md](docs/remote-format.md)), the
`s3://` and `layr+https://` remotes, and `layr mount`.

## Documentation

| Topic | Page |
|---|---|
| Every command, revisions, workflow, Git in a line | [docs/commands.md](docs/commands.md) |
| Refs, changes, attestations, protection, the merge queue, probes, views, bisect, secret checks | [docs/review.md](docs/review.md) |
| The merge engine, derived files, batch lines, the conflict radar | [docs/merge.md](docs/merge.md) |
| Peers, encrypted store remotes, the disk, replication | [docs/remotes.md](docs/remotes.md) |
| Runnables, OCI images, the registry, Helm charts | [docs/oci.md](docs/oci.md) |
| Security model, storage and consistency | [docs/security.md](docs/security.md) |
| Measurements | [docs/performance.md](docs/performance.md) |
| Overlays (private files in shared lines) | [docs/overlays.md](docs/overlays.md) |
| What this version does not do | [docs/limits.md](docs/limits.md) |
| Design and its reasons | [docs/design.md](docs/design.md) |

## Build and test

```sh
cargo build --release
cargo test --workspace        # unit tests
sudo tests/run.sh             # integration tests: each file gets a fresh btrfs on a loop file
```

## License

MIT
