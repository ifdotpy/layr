<h1 align="center">Layr</h1>

<p align="center">
  <b>Version control that snapshots folders, not files.</b><br>
  Every line of work is a real folder. Every save is an instant, read-only copy of it.<br>
  Every step, by a person, a script or an AI agent, can be undone.
</p>

<p align="center">
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Status: alpha" src="https://img.shields.io/badge/status-alpha-orange">
  <img alt="Platforms: Linux and macOS" src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-lightgrey">
</p>

```console
$ layr line new fix                # a new working folder, ready in milliseconds
$ cd ../fix && vim src/parser.rs
$ layr save -am "Fix the parser"   # freeze the folder as a state
$ rm -rf src                       # a mistake: yours, or an AI agent's
$ layr restore .                   # and it is back
```

## Why Layr

Git was built for one person, one working copy, and files that change by hand. Today a project
has a dozen agents working at once, each with its own folder, its own build cache, and shell
commands that change files behind the editor's back. Layr is built for that.

- **Parallel work is cheap.** A new line is a file system snapshot: constant time, no copy, the
  build cache comes along. 16 lines with 128 MiB caches take **153 MiB** in Layr and
  **2,508 MiB** as Git worktrees ([measured](docs/performance.md)).
- **Nothing gets lost.** Layr snapshots the folder itself, so it sees what `rm -rf`, `sed -i` or a
  code generator did. Every operation is a signed record, and `layr undo` reverses it.
- **You ship what you checked.** Checks, approvals and deploys name one exact state. Change one
  byte and the approval no longer applies. A merge queue keeps `main` green.
- **Secrets stay out.** Private files live in overlays that other readers never see, and Layr
  refuses any save, push or export that contains the value of a known secret.
- **Copies are encrypted and yours.** Push to a peer, a folder or your own S3 bucket. Data is
  encrypted with `age` before it leaves the machine.

## How it works

Layr has five ideas. Everything else is built from them.

```text
  line   main/              a real folder you work in
           │ save
           ▼
  state  7d3e               a frozen, read-only copy of that folder
           ▲        ▲
           │        └─ attestation   a signed note about one state: "tests passed", "approved"
           └────────── ref           a name for a state: env/prod, release/1.4.2

  remote                    another place with the same states: a peer, a folder, S3, Layr Cloud
```

Lines and states are btrfs subvolumes and snapshots, so creating one costs the same for ten files
or ten million. Changes between states come from file system metadata in time proportional to
the change, not to the project. A root service performs every operation, checks who asks, and
signs a record for it. The [design document](docs/design.md) explains each choice.

## Install

```sh
cargo install --git https://github.com/ifdotpy/layr layr
```

| Platform | Store on | Notes |
|---|---|---|
| Linux | btrfs (a loop file works) | Needs `btrfs-progs`, `git`, `zstd`. Run the service as root: `sudo layr daemon &` |
| macOS | a case-sensitive APFS volume | Set `LAYR_ROOT` to a folder on that volume. Commands run as you; the root service is optional ([details](docs/apfs.md)) |
| Linux | ext4 or XFS, through OverlayFS | Build with `--features overlayfs`; needs root mount permission ([details](docs/overlayfs-backend.md)) |

All three keep the same store and record formats, so machines on different platforms sync with
each other.

## Quick start

```sh
sudo layr init app --from ~/src/app --owner alice     # import a folder; main is protected
cd /var/lib/layr/projects/app/lines/main
layr line new topic                                     # a new line next to main
cd ../topic && $EDITOR src/lib.rs
layr status && layr diff
layr save -am "Fix the parser"                          # a new state
cd ../main
layr merge topic --expect "$(layr rev-parse topic)"     # exactly the state you reviewed
layr git push origin main                               # mirror to GitHub when you want
```

## If you know Git

| Git | Layr | Difference |
|---|---|---|
| branch + worktree | **line** | One thing: a line is its own folder, with its own caches |
| commit | **state** (`layr save -m`) | A snapshot of the whole folder, signed by the machine |
| `HEAD` | `head` | |
| `git stash` | `layr park` | |
| `cherry-pick`, `rebase`, `revert` | `pick`, `replay`, `invert` | Conflicts never stop the operation; they are part of the new state |
| tag | **ref** | Moves only with compare-and-swap (`--expect`), keeps its history (`env/prod@{1}`) |
| CI status, PR approval | **attestation** | Signed, stored with the state, bound to its exact content |
| reflog | `layr op log`, `layr undo` | Covers every operation, not only branch moves |

`diff`, `status --porcelain`, `show` and `log --format` print Git's formats byte for byte, so
editors and tools that read Git output keep working.

## A tour

| | |
|---|---|
| **Undo what an agent did** | `layr op log`, `layr restore --source <state> .` ([review](docs/review.md)) |
| **Find the step that broke it** | `layr bisect run --auto -- cargo test` searches every saved step, not only states with messages |
| **Merge lockfiles by rebuilding them** | `package-lock.json derive=npm` in `.layrattributes` ([merge](docs/merge.md)) |
| **Review behavior** | `layr probe run`, `layr change show` compare API answers, metrics and files between exact states |
| **Protect production** | `layr protect env/prod --check unit --approvals 1`, `layr queue run` |
| **Run any ref as a container** | images and Helm charts from `layr.toml`, served by the daemon ([OCI](docs/oci.md)) |
| **Keep encrypted copies** | `layr remote add vault s3://bucket/app`, `layr push vault`, `layr mount vault` ([remotes](docs/remotes.md)) |
| **Share files with some readers only** | overlays: private files in a shared line ([overlays](docs/overlays.md)) |

## Status

Layr is **alpha**. The btrfs backend is the supported one and has an integration suite of more
than a thousand checks that runs on real btrfs file systems; the core commands also run on APFS.
The store format and command names may still change. There has been no external security audit.
See [what this version does not do](docs/limits.md).

## Layr Cloud

[Layr Cloud](https://github.com/ifdotpy/layr-cloud) runs Layr in your own AWS account: an
encrypted S3 store per project, a gateway for machines without AWS credentials, and a host that
runs the merge queue and the registry. Layr talks to it as an ordinary remote (`layr+https://`).

## Documentation

[Commands](docs/commands.md) ·
[Review and delivery](docs/review.md) ·
[Merging](docs/merge.md) ·
[Remotes](docs/remotes.md) ·
[Containers](docs/oci.md) ·
[Overlays](docs/overlays.md) ·
[Security](docs/security.md) ·
[Performance](docs/performance.md) ·
[Design](docs/design.md)

## Contributing

```sh
cargo test --workspace      # unit tests
sudo tests/run.sh           # integration tests: each file gets a fresh btrfs on a loop file
```

Unsafe code is limited to `src/sys.rs`. Commit messages say what changed and why, in plain words.

## License

[MIT](LICENSE)
