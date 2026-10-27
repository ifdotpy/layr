# Not in this version

This page is part of the [Layr documentation](../README.md#documentation).

## Not in this version

- APFS volume snapshots: they need root and an Apple entitlement. Current snapshots use verified directory clones.
  APFS sends against a parent hold only the differences, but no other clone sources and no compressed streams.
- Overlays beyond the first slice: routing patterns, ephemeral mode, folder overlays, encrypted
  remote copies and replication of overlays (`docs/overlays.md`, section 9.7).
- Structured (AST) merge; format drivers can be plugged in through `merge.driver.<name>`.
- `layrhub`: records, ids and stream transport are ready for it; the protocol is not.
- Content checks of replicated states: a received state must be the subvolume its signed record
  names (`received_uuid`), but its file content is trusted together with the machine that made
  it. Cache snapshots are checked the same way through the signed `save.caches` record.

### Git folder imports

An initialized submodule keeps its original `sub/.git` file during `init --from`.
Its `gitdir` path can become invalid in the imported folder.

Source inspection suggests a Linux service import can fail for a caller without root access.
`copy_folder` creates directories as root, then copies files with the caller's permissions.
The caller can lack write access to these directories.
This service path has no live verification for this case.

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
The [store format](remote-format.md) describes the objects and key slots.

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

### Read a store as a disk

`layr mount <remote>` reads store format v2 and serves a read-only WebDAV disk at `http://127.0.0.1:8095/`.
Use a named project remote or a store URL (`file://`, `s3://`, or `layr+https://`).
A URL works without a local project, a snapshot filesystem, root, or the Layr service.
S3 credentials come from the process environment.

Each top-level folder shows one live line. The disk selects the newest available signed
head across machines. It shows a newer automatic state when that state is available.
The folder name replaces `/` with `%2F` and `%` with `%25`.
State information is in `/.layr/<encoded-line>.json`. The source tree has no added files.
The view refreshes every 120 seconds. An unsuccessful refresh keeps the previous view.

A machine slot opens the store with the local machine key. Without a machine slot, use
`layr mount <url> --paper-key-file <path>`. The file must belong to the current user and
have mode `0600`. You can instead pipe the words with `--paper-key-stdin`.
The command never prints the key. On macOS it saves the unlocked store identity in the
login Keychain, so later starts need no paper file. On Linux the identity stays in memory.

The plaintext cache uses SHA-256 file names in `~/Library/Caches/layr/mount` on macOS,
or `$XDG_CACHE_HOME/layr/mount` (`~/.cache/layr/mount` by default) on Linux.
The folder has mode `0700`. Cache files have mode `0600`.
The default limit is 1 GiB. Use `--cache-size <bytes>` to change it, or `0` to disable it.
The command verifies cached bytes before it serves them.

On macOS, run `layr mount --install <url>` to start the disk at login and mount it at
`~/Layr`. After one successful start, `layr mount --install` uses the saved remote.
`layr mount --remove` stops the agent and removes it.
The agent stores no AWS credentials or paper key in its arguments or property list.
Use `--listen <loopback-IP>:<port>` for another local port. Remote listen addresses are refused.

