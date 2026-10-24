# Remotes, store copies and the disk

This page is part of the [Layr documentation](../README.md#documentation).

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
The CLI uses `cache` and `--caches`. The old `layer` and `--layers` names fail and show the new names.
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
[final timing test](../benchmarks/results/btrfs-opt-isolated-10000.json),
[sync and extent checks](../benchmarks/results/btrfs-opt-sync.json),
[initial timing test](../benchmarks/results/btrfs-opt-initial-10000.json),
[shared filesystem repeat](../benchmarks/results/btrfs-opt-shared-repeat-10000.json),
and [shared filesystem CPU measurements](../benchmarks/results/btrfs-opt-shared-cpu-10000.json).
The earlier runs use intermediate builds; their hashes identify each build.


## Person keys and store access

A person key opens every store that has a slot for that person.
Run these commands on your own machine.
The person files live in `~/.config/layr/person/`.
Set `LAYR_PERSON_DIR` to select another folder.
The files contain a public recipient, encrypted paper slot, and encrypted index.
Secret files have mode `0600`.

```sh
export LAYR_PERSON_LOGIN=ann@example.com
layr person keys --paper-key-out /private/path/person-words
export LAYR_PERSON_KEY_FILE=/private/path/person-words
layr person show
```

The output file contains 24 words and must not exist before the command.
Without `--paper-key-out`, a private terminal shows the words once.
Write the words on paper and remove the file after you check them.
The command never stores the words in the person folder unless you explicitly name that output path.
`LAYR_PERSON_KEY_FILE` can contain the words or the person age identity.
It must be a regular file with mode `0600`, owned by the caller.
The person identity stays in memory after the paper slot opens.
Paper keys must never go to executor machines.

The owner grants their own login first.
The first person slot hides the store from gateway logins without a slot.
File and S3 grants require `LAYR_PERSON_LOGIN`.
A gateway URL uses only the login that the gateway gets from tailscaled.

```sh
layr remote grant cloud --person ann@example.com --recipient age1... --invite-out /private/path/ann.age
layr person accept --invite-file /private/path/ann.age
layr remote grant cloud --person carol@example.com --recipient age1... --invite-out /private/path/carol.age
```

Use each person's actual recipient from `layr person show`.
Give Carol the encrypted invite through a channel that you trust.
Carol runs the following commands with her own person files and key:

```sh
export LAYR_PERSON_LOGIN=carol@example.com
export LAYR_PERSON_KEY_FILE=/private/path/carol-words
layr person accept --invite-file /private/path/carol.age
layr clone file:///path/to/store copy
layr person show
layr mount file:///path/to/store
```

An invite does not change the index until the person accepts it.
For an invite file, check the project and recipient with the owner before acceptance.
The owner needs a machine slot, the store paper key, or their own accepted person slot to grant another person.
For the store paper key, set `LAYR_PAPER_KEY_FILE`.
A new person key gets a new slot. An existing slot returns `already granted` without replacement.
The command sends the invite again to the gateway inbox or the new `--invite-out` path.

For a gateway, register the same person key and use its inbox:

```sh
layr person keys --site https://gateway.example.com
layr person accept --site https://gateway.example.com --invite 1792825920000-0123abcd
```

A later `person keys --site` registers the existing recipient and encrypted paper slot without new words.
Gateway grants send the invite to the recipient's inbox.
Gateway acceptance checks the store descriptor and person slot before it writes the index.
A concurrent index change returns HTTP 409 and requires another read.
`person show --site` prints the gateway login.

The index preserves the Desk JSON and MAC format.
For a v2 store, each pin contains its project UUID and recipient.
`writer` and `signedFrom` are null because v2 tree signatures use machine keys.
Clone, fetch, and mount refuse a changed pin before the person slot opens.
The error names the pinned value and the value in `store.json`.
`person show` lists accepted stores when `LAYR_PERSON_KEY_FILE` opens the index.

For v2 stores, `fp` is the first 16 hexadecimal characters of SHA-256 of the person recipient string.
The opener computes this value and reads its exact slot.
The store creates every slot conditionally and never replaces an object.
Old person keys retain access to data that they could already read.
Desk v1 import reads `slots/person-<login>.age`.

## Private overlays

Push and sync include the project's overlays.
A store encrypts each overlay with its own age layer key.
A machine reader needs its machine slot. A person reader needs its person slot.
The person slot name includes the recipient fingerprint, as store person slots do.
Local user and group readers remain service policy only.

Fetch and clone report `overlay <name>: no key on this machine` when no slot opens.
The shared files still import.
Mount shows an overlay in the line view only when a slot opens.
A machine without a layer slot can push to the public, signed layer recipient.

Reader removal starts a new epoch on the next push.
Earlier objects remain unchanged. A removed reader retains access to data already copied.
Peer sync sends an overlay only to a permitted peer machine.
The receiving service enforces the overlay readers.
See [overlays](overlays.md#10-overlays-in-stores-and-peer-sync) for principals, peer settings and the paper slot.
