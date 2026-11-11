# Remotes, store copies and the disk

This page is part of the [Layr documentation](../README.md#documentation).

## Remotes and replication

Add a peer with `layr remote add work ssh://host/project`.
Run `layr sync work` to exchange signed records and missing states in both directions.
Layr starts `ssh host layr serve-peer project`.
The internal peer command does not appear in help.
Use `--auto --caches` to include automatic states and saved caches.
Owner decision 2026-11-11: a cache is an ordinary overlay, and the cache flags go away. `fetch` then takes lines and overlays: the history of the chosen lines, and only their parts on the chosen overlays ([overlays](overlays.md) section 1). Without `--line` and `--overlay`, `fetch` takes the whole repository, and `--caches`, `--caches-of` and `--no-caches` select caches. `layr fetch <store remote> --line <name>[,<name>] --overlay <layer>[,<layer>]` and the same flags on `push` are the first built part of this rule: `--line` takes the history of those lines (their heads with `--heads`) and, in each overlay, the history of their overlay lines; `--overlay` takes only those layers, and `base` names the base. A state left out stays on the remote and comes on first read. Example for a merge runner: `layr fetch cloud --line main --overlay base,cache.target`.

A fetch that finds a line whose head tree is not on the remote yet prints `layr: line <name>: head state <id> is not in the store yet` on stderr and goes on with exit status 0. It fails with exit status 2 only when the caller asked for that line (`--line`, `--only-line`). A push to a store remote also sends a state that is here only as objects (its native copy retired after `layr migrate`).
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

Create a new store from your private terminal:

```sh
layr remote add archive file:///path --paper-key-tty
```

The command shows 24 paper words once on the controlling terminal (`/dev/tty`).
Save the words privately. Type the final word at the confirmation prompt.
Layr publishes the encrypted slots and store descriptor only after that confirmation.
An incorrect answer cancels creation without publishing store objects.
The words do not enter stdout, stderr, a key file, project records or configuration.
Only the public paper recipient is pinned in configuration.
Both stdin and stdout must be terminals. Stdin must use the controlling terminal.
Pipes, output redirection and sessions without a controlling terminal are refused.
Run this command locally. A background service without your controlling terminal cannot show the key.
An existing store cannot show its paper key again.
If publication fails after confirmation, retain the saved words for recovery.

For file output, use `layr remote add archive file:///path --paper-key-out /private/path/key.txt`.
The output file must not exist. It must be outside the store and all line folders.
Layr creates the file as the caller with mode `0600`.
Write the words on paper. Delete the file after you check the paper copy.
File output does not print the words or put them in project records or configuration.
Use either `--paper-key-tty` or `--paper-key-out` for a new store.
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

A Gateway restart can invalidate a page token for an object list.
The client repeats the list once after `invalid listing continuation`.
It discards the partial result and keeps the same access mode.
A second token failure stops the operation.

## Project modes

A store has one of three modes (see docs/cloud-rfc.md 7.0):

| Mode | The server reads the code | Keys | Who writes |
|---|---|---|---|
| `secret` | no | vault key, slots, paper key, epochs | trusted machines |
| `private` | yes | none | trusted machines; the Layr Cloud API adds its access list |
| `public` | yes | none | trusted machines; the Layr Cloud API adds its access list |

Choose the mode when you create the store.
A new store is `private`, unless you ask for a paper key: `--paper-key-out` or `--paper-key-tty` makes a `secret` store.

```sh
layr remote add cloud s3://bucket/project
layr remote add vault s3://bucket/secret --mode secret --paper-key-out /private/path/words
layr clone layr+https://cloud.example/app --mode private
```

To clone only the `main` working folder and its head, use:

```sh
layr clone layr+https://cloud.example/app app --heads --only-line main
```

The clone keeps the project records. Other states stay on the remote. A command that needs a
state can restore it from that remote. For example, `layr line new` can use another line's state.
Use `--only-line` with `--heads`. Without `--only-line`, clone creates all available line folders.

A new source clone selects the creator's line heads in a legacy store. Machines in the
creator's `admin.machines` setting have the same rights. A later adoption by another
machine does not replace those heads. A V1 clone uses the published server line refs
and requires their signed state records before it writes a working folder. States
that only appear in V1 records are not published line refs.

A configured `queue.merge-machine` advances legacy `main` only through a matching
successful queue result. Its later private adoption does not move `main` back.

Clone uses one head snapshot for state download and working folders. A later fetch
can finish missing clone folders. It does not replace an existing working folder or
its local head. Other machines' per-machine heads remain in the local history.
Use `layr fetch origin --heads --only-line main` to fetch the selected head again.

A `private` or `public` store keeps plain objects and records, and its `store.json` names the mode.
It has no slots and no paper key, so `--mode private --paper-key-out` is refused.
These modes are meant for the Layr Cloud API, where the server reads the code and checks each writer.
On a bare folder or bucket they only drop the encryption:
anyone who can read the bucket reads the code, and a fetch still imports records of trusted machines only.
`layr remote trust` records trust in history and writes no slot.
`layr index write` is for `secret` stores only; the server builds the indexes of the other modes.

`remote add --mode` and `clone --mode` record the mode in `cloud.remote.<name>.mode`, and the store pin keeps the mode.
A store whose `store.json` later names another mode is refused, so a writer of the bucket cannot turn a `secret`
project into a plain one.
Use `--mode secret` when you clone a project that must stay encrypted.

## Layr Cloud API v1 (Public and Private)

A `layr+https://<host>/<project>` remote whose mode is `private` or `public` talks to the Layr Cloud
API v1 (layr-cloud `docs/api.md`): base `https://<host>/api/v1/projects/<project>`.
A remote with `--mode secret` keeps the encrypted store routes of the gateway.
A `file://` or `s3://` store of any mode keeps the store layout above.

An existing encrypted store pin keeps the older gateway protocol. For a new URL without a mode,
the client checks the V1 project and records routes. If V1 is absent, it checks the older store
descriptor before it uses that protocol. A denied V1 request does not select the older protocol.
A pinned V1 remote never falls back. Changing a pin to another format requires a verified migration.

```sh
layr remote add cloud layr+https://cloud.layr.ws/app            # Private by default
layr login cloud                                                # a person: sign in, register this machine
LAYR_GATEWAY_TOKEN_FILE=/private/token layr push cloud          # a machine with an admin-issued token
```

- **Requests.** Each request sends `Authorization: Bearer <token>` (when there is one) and
  `Layr-Client: <layr version>`. The token is the file in `LAYR_GATEWAY_TOKEN_FILE` (a machine or job
  token, owner-only mode), else the person session that `layr login` saved in
  `~/.config/layr/cloud/<host>.json`. A `426` answer stops the command with the minimum version
  the server needs; there is no compatibility window. Errors name the `Layr-Server` version.
- **Machine key.** Before the first record upload, the client registers the public key of this
  machine: `PUT /api/v1/machines/<machine id>` `{"public_key":"<64 hex>"}`. The same key again is
  `200`; another key for the same machine is `409`. The server checks each record segment of the
  machine against this key.
- **Person login.** `layr login <remote>`: `POST /api/v1/login` gives `{"code","link","expires"}`;
  the person opens the link and signs in with a passkey; the client polls
  `POST /api/v1/login/<code>` (`202` while waiting, then `{"login","token","expires"}`), saves the
  session, and registers this machine's key with it.
- **Person requests.** Set `LAYR_PERSON_KEY_FILE` to the person key or paper key and
  `LAYR_PERSON_LOGIN` to the signed-in login. `layr request approve <state> --person` sends a
  signed request. A merge executor applies it with `layr requests apply --no-run-queue` and
  publishes the resulting record. API v1 keeps these requests as plain JSON under project read
  access. Public projects expose them to the public. Secret stores retain encrypted requests.

### Objects, packs and states

States use the object format of docs/objects.md: a state is a tree and a times table, files are
blobs, and objects travel in packs. The ids are the typed ids of docs/objects.md section 2 (a blob
id equals the Git SHA-256 blob id); the server checks every plain id.

- A push writes each state into the project's object store (`meta/objects`, the store of
  `layr objects`): only the paths that changed since the first parent when the parent is there,
  new blobs cold (a delta against the parent's blob, else a zstd frame).
- It asks `POST /objects/missing` with the ids of the times tables, trees and blobs that the states
  reach, and sends the missing ones in transfer packs (docs/objects.md section 6; raw entries
  only, so the server checks each entry without another object): the data file as an `opaque`
  object (`PUT /objects/<pack id>` with `Layr-Object-Type: opaque`, in parts over 64 MiB), then
  its index (`PUT /packs/<pack id>`). A pack holds about 56 MiB of plain bytes.
- Then `PUT /states/<state>` `{"times":"<id>","tree":"<id>"}` publishes each state;
  `GET /states/<state>` gives the body back.
- A fetch reads the state bodies, asks `POST /objects/locate` `{"ids":[...]}` (answer
  `{"packs":{"<object id>":"<pack id>"}}`) for the missing trees and times tables, then for the
  missing blobs, downloads those packs (`GET /objects/<pack id>`, `GET /packs/<pack id>`), adds
  each to the object store after its checks (index, pack id, every entry against its id), and
  checks each state out with its times. For a Cloud v1 whole-tree times table, the client checks
  its shape against the tree and converts it to local version 2 folder tables. The server state and
  its original times object do not change.
- Secret stores and bare folder or bucket stores keep the store v2 trees.
- `POST /states/missing` `{"ids":[...]}` (at most 1000) gives `{"missing":[...]}`. A push asks
  once for the tips and every state that may go up, and a fetch once for the states it lacks, so
  neither sends a request per state.
- Objects over 64 MiB go up in parts (`POST /objects/<id>/uploads`, then each part, then
  `complete`). Before a large upload the client asks `POST /objects/missing`, so an object that the
  project already holds is not sent again.

### Records and refs

- Record segments go up plain: `POST /records/<machine>/<first>-<last>`. A machine uploads only
  its own segments (the token belongs to one machine). A fetch takes the records of every writer
  that the server serves: the server access list, not local machine trust, decides who writes
  there. It does not widen local trust: a machine that this project does not trust yet cannot
  change trust here (its `machine.trust`, `machine.untrust` and `trust.delegate` records have no
  effect), so peer sync, cache readers and other remotes keep the local trusted set. The key of
  each writer is pinned per remote on first sight (`writer-keys.json` of the remote folder): a
  later key for the same machine stops the fetch. A clone trusts the project's own trust chain
  only: the creator and the machines that a member trusts by a signed `machine.trust` record.
- The server ref is the truth for the position of a line. After records and states, a push moves
  each changed line with one `POST /refs/cas`: `name` `lines/<line id>`, `expect` the target this
  machine last read from the server (null for a new line), `target` the local head, and `record`
  the id of the newest record of this machine that moved the line. Only lines that this machine
  moved go up. A head that does not contain the `expect` state is refused before the request, as
  a Git push without force: merge what the server has first. A `409 conflict` names the lines that
  moved on the server: run `layr fetch <remote>` and push again. A fetch reads
  `GET /refs?prefix=lines/`, keeps the targets in `refs.json` of the remote folder as the next
  `expect`, and names each line whose head here differs from the server.
- Overlays: an overlay whose policy lets everyone (`*`) read it goes to `/overlays/<name>/`
  (records, states, refs) as plain objects in the project's object store: no overlay keys, slots
  or epochs. Any other overlay (a cache, whose readers are the trusted machines; a reader list; a
  hidden overlay) stays on this machine and the push names it, because the server serves a plain
  layer to every reader of the project until it keeps per-overlay readers.
- The client does not sweep, and `layr remote gc` and `verify` refuse: garbage collection is
  server-side, and the server checks each object against its id on upload. Git packs of imported
  commits do not go up: the server mirrors these projects to GitHub.

## Push roles

Each remote has a push role: `layr config remote.<name>.push required|optional|off`.
A store remote is `required` by default. A git remote is `optional` by default.

- `layr push` without a name pushes to every remote that is not `off`, required remotes first.
- `layr push cloud origin` pushes only to the named remotes.
- A store remote gets records and states. A git remote gets the line of the command, every protected line,
  and each line whose earlier push to that remote failed, each to the branch of its name.
- A required failure fails the command (exit 1).
- An optional failure prints `warning: push to <remote> (<lines>) failed`, and the command continues.
  Layr records the lines in `meta/push-pending.json`. The next push to that remote tries them again.
- With `--json` the report is `{"remotes":[...]}`. Each entry has `name`, `kind`, `role`, `pushed`, `failed` and `report`.

`queue.push` takes a comma list of remotes (`layr config queue.push cloud,origin`).
A queue pass pushes the merged lines to those remotes. Unset `queue.push` means no push.

```sh
layr config remote.origin.push optional
layr push
```

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
The CLI uses `--caches`. The old `layer` and `--layers` names are unknown.
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

## Store disks

`layr mount` serves a read-only WebDAV disk on the loopback address.
Each line folder of the disk shows the newest state of that line on any machine, automatic saves included.
A clone checks out the creator's head of each line instead (or the main head of `queue.merge-machine`).
A save that another machine made on its own copy of a line is visible in the mount, and a clone reads it by its id.
Each line has a folder. The `.layr` folder contains state metadata.
On macOS, the login Keychain keeps each store identity after its first mount.
The Keychain account is the SHA-256 of the store URL.

```sh
layr mount --unlock cloud --paper-key-file /private/path/store-words
layr mount --status --json
layr mount cloud --listen 127.0.0.1:8095
layr mount --lock cloud
```

Unlock checks the store pin and saves the identity without a network server.
It can use the machine slot, a paper key, or `LAYR_PERSON_KEY_FILE`.
Use `--paper-key-stdin` for a pipe instead of a file.
A paper key file must have mode `0600` and belong to the caller.
Lock removes the Keychain item. File requests return HTTP 423 until the next unlock.
The single-store disk lists only folders while locked.
Status prints the URL, lock state, service state, and active server address.
These three Keychain flags exit with code 2 on Linux.

```sh
layr mount --all
layr mount --install --all
layr mount --remove --all
```

The all-store disk has one folder per known store.
Remote names identify the folders. Duplicate names get a number suffix.
A locked store contains `LOCKED.txt` with its unlock command.
The macOS service `ws.layr.mount` starts one Finder disk named `Layr` at login.
`--install --all` replaces a single-store install.
`--install <remote>` replaces an all-store install.
Each command prints the mode that it replaces.
`--remove --all` exits with code 2 if the installed mode is not all stores.
Status includes the installed mode. Its JSON object contains `installed_mode` and `stores`.
Single-store `--install` and `--remove` retain their existing use.
Installed agents use only the Keychain to open store data.
A lock remains in effect after a restart or reboot until `layr mount --unlock`.
A direct `layr mount <remote>` retains its automatic slot access and saves the Keychain item.
The server refuses a listen address outside the loopback interface.

## AWS profiles and login

S3 credentials first use `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
Temporary credentials also use `AWS_SESSION_TOKEN`.
Without these keys, Layr uses `AWS_PROFILE`, then the remote profile setting, then `default`.

```sh
layr config remote.cloud.aws-profile work
layr remote login cloud
layr push cloud
```

The profile name can be in Layr config. Credentials cannot.
Profiles support static keys and IAM Identity Center (SSO).
Layr reads `~/.aws/config` and `~/.aws/credentials`.
`AWS_CONFIG_FILE` and `AWS_SHARED_CREDENTIALS_FILE` select other files.
`AWS_REGION` or `AWS_DEFAULT_REGION` overrides the profile region.
Layr refuses `credential_process` because it starts a program.
Layr also refuses a profile that assumes a role (`role_arn`, `source_profile`, `credential_source`,
`web_identity_token_file`): its keys would act as another identity without saying so.

SSO profiles can use `sso_session` or the older `sso_start_url` fields.
`remote login` prints the device authorization URL and code, then waits for approval.
The token goes to the standard AWS SSO cache with mode `0600`.
Layr refreshes a token near its expiry and requests the selected role credentials.
`AWS_ENDPOINT_URL_SSO` and `AWS_ENDPOINT_URL_SSO_OIDC` select test endpoints.
Layr never writes AWS keys or tokens to its config, records, or logs.

Give a store address when you accept a file invite:

```sh
layr person accept --invite-file /private/path/invite.age --remote s3://bucket/project
```

Layr checks the descriptor and person slot before it saves the address.
Gateway acceptance saves its store address automatically.
The local `addresses.json` file has mode `0600` beside the encrypted person index.
The Desk index and its message authentication code (MAC) stay unchanged.
Set `LAYR_PERSON_KEY_FILE` so mount can read accepted person pins.
A pin without a known address stays off the disk.
Status prints `no address` and the command to accept that pin again with `--remote`.
A person pin without a named remote uses `<project>-<short project id>` as its folder name.
The service keeps the person key file path and login, when set, for index access after login.
It stores no person key text.

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

The overlay creator machine is its first key manager.
Only key managers can change readers or layer keys.
Use `layr overlay managers <name> machine:<id>...` to add a second manager.
Other trusted machines can upload to the published layer recipient.
