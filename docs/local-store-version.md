# Local store version

Each local store has a `store-version.json` file at its root.
This file is independent of the encrypted remote store format.

```json
{"format":1,"version":"0.4.0","build":"<source identity>"}
```

`format` controls compatibility.
`version` and `build` identify the binary that creates or explicitly upgrades the marker.
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

The command does not call `Store::open`.
It does not create a root, a marker, a lock, or a database.
It returns status 0 for valid reports, including incompatible stores.
A malformed or unreadable marker returns status 128.
`version`, `--version`, `help`, `--help`, and `-h` also avoid `Store::open`.

| State | Meaning | Compatible |
| --- | --- | --- |
| `absent` | The root does not exist. | Yes, the first normal command can create it. |
| `empty` | The root contains no store data. | Yes, the first normal command can initialize it. |
| `legacy` | The root contains data but has no marker. | No. |
| `compatible` | Both format numbers are equal. | Yes. |
| `incompatible` | The format numbers differ. | No. |

`store` is null for absent, empty, and legacy roots.
Legacy roots and explicit format 0 markers report this supported upgrade:

```json
{"metadataOnly":true,"fromFormat":0,"toFormat":1}
```

Other states report `upgrade: null`.
This classification describes changes to the version marker and its lock file.
The explicit upgrade does not change machine identity files or project data.

## Upgrade explicitly

Stop all commands and daemons that use the store.
Then run:

```sh
layr --root /path/to/store store-version --upgrade
```

The command adopts a nonempty unversioned store as format 1.
An absent or empty root refuses this command without a write.
A normal command initializes an empty root.
It also accepts an explicit format 0 marker.
It preserves the existing store data and records the current binary identity.
The project database retains its existing schema migration mechanism.
The command refuses other incompatible formats and never downgrades a newer store.

Every open store holds a shared lease on `.layr-version.lock`.
The daemon holds this lease for its lifetime.
An upgrade requires an exclusive lease and refuses an active store.
The binary rechecks the marker after it acquires the lease.
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
An explicit upgrade cannot detect an active older binary that takes no version lease.

Studio must inspect its bundled and developer binaries before it shares a root.
It must refuse a developer binary that has no `store-version` command.
Studio can require equal package versions as its own policy.
Layr itself enforces only the local format number.

The build identity comes from `git describe --always --dirty --exclude '*'`.
A source archive can supply `LAYR_BUILD_ID` at build time.
Without either source, the identity is `unknown`.
