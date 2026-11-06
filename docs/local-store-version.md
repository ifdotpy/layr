# Local store version

Each local store has a `store-version.json` file at its root.
This file is independent of the encrypted remote store format.

```json
{"format":1,"version":"0.4.0","build":"<source identity>"}
```

`format` controls compatibility.
`version` and `build` identify the binary that creates the marker.
Later compatible binaries do not replace this identity.
Different package versions or build identities can use the same format.

Before machine initialization, recovery, or project access, `Store::open` compares both formats.
A mismatch returns status 128 before the command changes store data.
The error names the store format, the binary format, both package versions, and both build identities.
This rule also applies to commands that look like reads.
These commands can otherwise create snapshots or migrate a database.

## Inspect without a write

```sh
layr --root /path/to/store store-version --json
```

Example output:

```json
{
  "binary": {"format":1,"version":"0.4.0","build":"<source identity>"},
  "store": {"format":1,"version":"0.4.0","build":"<creator identity>"},
  "compatible": true,
  "state": "compatible",
  "upgrade": null
}
```

When a daemon of this user answers (the LaunchAgent socket on macOS, `LAYR_SOCKET` or the service
socket elsewhere), the report also has `daemon`: its `format`, `version` and `build`. The daemon can
run an older binary of the same format after an install without a restart; compare its `build` with
`binary.build`. A busy daemon gives `{"busy": true}`. A daemon too old to report its version gives
`{"version": null}`. Without a daemon the field is absent. `layr follow` warns when the daemon runs
another build than the client.

The command does not call `Store::open`.
It does not create a root, a marker, a lock, or a database.
It returns status 0 for valid reports, including incompatible stores.
A malformed or unreadable marker returns status 128.
`version`, `--version`, `help`, `--help`, and `-h` also avoid `Store::open`.

| State | Meaning | Compatible |
| --- | --- | --- |
| `absent` | The root does not exist. | Yes, the first normal command can create it. |
| `empty` | The root contains no store data. | Yes, the first normal command can initialize it. |
| `unmarked` | The root has no marker and has the layout of format 1 (a machine identity). | Yes, the next open writes the marker. |
| `compatible` | Both format numbers are equal. | Yes. |
| `incompatible` | The format numbers differ, or the root has no marker and no machine identity. | No. |

`store` is null for absent, empty, and unmarked roots.
An unmarked root reports the marker write that the next open makes:

```json
{"metadataOnly":true,"fromFormat":0,"toFormat":1}
```

Other states report `upgrade: null`.
This classification describes changes to the version marker and its lock file.
Writing the marker does not change machine identity files or project data.

## Unmarked stores (format 0)

A store without a marker is format 0.
Its layout is the same as format 1, so no data changes.
The first command that opens it writes the marker. No extra step is needed.

- A root without `machine/id` is not a layr store. It is refused, and no file changes.
- The marker is written under the exclusive lease, through a temporary file and a rename.
- Parallel first commands wait for the lease. The first one writes the marker, and the others find it.
- A root that is not writable is refused. Give write access to the store root and run the command again.
- An explicit marker with another format is refused. It is never rewritten.

Every open store holds a shared lease on `.layr-version.lock`.
The daemon holds this lease for its lifetime.
The first open of an unmarked store takes the exclusive lease only to write the marker.
It rechecks the marker after it acquires the lease.
Store initialization also holds an exclusive `.layr-machine.lock` lease through the identity read.
It applies even when a format marker already exists.
Complete identity files publish once through an atomic hard link.
An existing ID, name, or key is never replaced.
A missing key with existing project data refuses access and requires identity restoration.

The client and daemon exchange their format descriptors before a command uses the daemon.
The exchange uses the same connection as the command.
Either format mismatch refuses the command.
The daemon also refuses clients that supply no format descriptor.
A new client refuses an older daemon that has no version exchange.

## Older binaries

Older binaries that ignore this marker cannot enforce the guard.
Upgrade all binaries and daemons before they share the store.
The marker cannot stop a binary that never reads it.
The marker write cannot detect an active older binary that takes no version lease.

Studio must inspect its bundled and developer binaries before it shares a root.
It must refuse a developer binary that has no `store-version` command.
Studio can require equal package versions as its own policy.
Layr itself enforces only the local format number.

The build identity and date come from `LAYR_BUILD_ID` and `LAYR_BUILD_DATE` at build time.
A release build sets both, for example `LAYR_BUILD_ID=$(git rev-parse --short HEAD)
LAYR_BUILD_DATE=$(date +%F) cargo build --release`. Without them, both are `dev`.
The build never reads `.git`: a watch of `.git/HEAD` rebuilt the binary in every new layr line.
