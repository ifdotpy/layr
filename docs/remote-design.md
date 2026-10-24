# Remotes: design decisions (lead)

Status: decided. This file overrides RFC 6.3–6.5 where they differ; each difference names its
reason. Workers implement this file; they do not change these decisions without the lead.

## Facts from the code that drive the design

- The truth of a project is the signed op log: each machine appends its own records with its
  own sequence numbers and Ed25519 key (`src/model.rs` `Record`, `src/db.rs` `records`). Line
  pointers are records; a line has one head per machine (`line_heads`), so two machines never
  overwrite each other's head.
- `sync` already replicates this log both ways and sends missing states with `btrfs send -p`
  (`src/cmd/replicate.rs`).
- A `gc` record from another machine does not delete local states (`src/db.rs`, `"gc"`, the
  `!foreign` check). Invariant 2 of RFC 12.4 holds at the database level; tests must prove it
  end to end.
- Layr Desk (`~/Projects/layr-desk`, to be deleted) stores one linear stream per project in
  S3: `states/<seq>.<attempt>.tree.age` and data parts, `age` encryption to a vault recipient,
  slots that hold the vault identity (paper key, passkeys), zstd file and tree deltas, a
  write-only writer that keeps its own last tree locally. Its code is the starting point for the
  store format; nothing in Layr may depend on that repository.

## Two kinds of remote

| URL | Kind | Other side | Protocol |
|---|---|---|---|
| `ssh://<host>/<project>` | peer | a Layr service | existing peer protocol: records, then `btrfs send` (`replicate.rs`) |
| `file:///<path>`, `s3://<bucket>/<prefix>` | store | plain storage, no Layr | store format v2 (below) |
| `oci://<host>/<repo>` | registry | an OCI registry | `push` of runnables only (workstream oci) |

A peer remote is fast and complete (caches, auto states, btrfs identity). A store remote works
without Layr on the other side, survives the loss of every machine, and is readable without btrfs
(Layr Cloud browse, restore on any machine).

## Store format v2

Layout under the remote root (`file://` folder or `s3://bucket/prefix`):

```text
store.json                                   plain: {"format": 2, "project": "<id>", "recipient": "age1…"}
slots/<name>.age                             vault identity encrypted to one opener (paper key, machine)
records/<machine>/<first>-<last>.age         one segment of a machine's signed records (JSONL), immutable
states/<state id>/<attempt>.tree.age         tree of one state, written last (the commit point of that attempt)
states/<state id>/<attempt>.<part>.data.age  data frames of that attempt
```

Decisions:

1. **No CAS refs in the store (differs from RFC 6.5).** Refs, lines, tags, attestations and
   access rules are records, so the store carries the record log. Each machine writes only its
   own segments; segment names are its sequence ranges. Every object is created with
   `If-None-Match: *` (S3) or an exclusive create (`file://`). No object is ever overwritten,
   so no `If-Match` CAS is needed. Concurrent writers do not conflict, as with `sync` today.
   Atomic moves of protected refs across machines become a Layr Cloud service function later.
2. **Lost response.** A record segment has a fixed name and immutable content: a 412 on retry
   means it already exists (record signatures prove the content when read). A state upload uses
   a random 8-hex `attempt`, as Desk does: a retry after an uncertain failure uses a new attempt;
   any attempt with a tree object is complete; readers take the first complete attempt in name
   order. Orphan data parts of incomplete attempts are removed by `gc --remote` after a grace
   period. **HMAC object names and the ciphertext digest of RFC 6.3 are dropped**: the attempt
   rule gives idempotent retries without them.
3. **Tree object:** Desk's header line plus zstd frame, extended: header
   `{"v": 2, "state": "<id>", "attempt": "<hex>", "tree": "full"|"delta", "base": ["<state id>",
   "<attempt>"], "parts": N, "machine": "<id>", "sig": "<ed25519 over the canonical tree and the
   header without sig>"}`. The canonical tree is Desk's `{"files": {"<path>": [mode, size,
   sha256, chain]}}` with chain links `["<state id>", "<attempt>", part, offset, length, kind]`.
   Readers verify the signature against the machine key from that machine's first record
   (the same trust rule as `sync`). Full tree at least every 32 states along a chain; at most 16
   file deltas per chain; zstd level 3; parts up to 16 MiB; multipart for large objects (port
   Desk's S3 code).
4. **What a push sends:** records the store lacks (from every machine the local log has), then
   the states those records need and the store lacks: named states, imports, parked states, merges; auto
   states with `--auto`; cache states with `--caches`. Files of private overlays are never sent
   in V1 (separate layer keys come later, RFC 7); the push reports what it left out.
5. **Delta bases and dedup.** The writer keeps, per store remote, a local index of what it has
   pushed: state id → attempt and canonical tree. A changed file gets a delta against the same
   path in the nearest pushed ancestor state; a file whose sha256 already has a chain in the
   index reuses that chain (dedup across lines). States pushed by another machine are bases only
   when this machine can read the store (it holds a slot).
6. **Keys (Desk model, no own crypto).** One vault identity per store remote (`age` X25519);
   `store.json` holds its recipient. Slots: `paper.age` (24-word paper key as in Desk) and
   `machine-<machine id>.age`, encrypted to the machine's existing Ed25519 key used as an `age`
   `ssh-ed25519` recipient. A machine without a slot is write-only: it can push, it cannot fetch
   (Desk's property that agents and a stolen VM disk cannot read the store). `layr remote trust
   <name> <machine>` adds a slot; removing a slot stops future reads only (RFC 7.3).
7. **Read:** `fetch` reads new record segments, verifies them like `sync`, appends them, and
   rebuilds the states the records need from trees and frames into btrfs snapshots (verify every
   file's sha256). `clone <url> [<folder>]` = new project + fetch + line folders.
8. **Retention and gc:** a store keeps everything by default. `layr gc --remote <name>` (needs a
   slot and delete rights) removes only orphan parts of incomplete attempts older than 24 hours.
   It never deletes states that records reference. Writer credentials should have no delete
   right (IAM policy as in Desk's template).
9. **Desk v1 import:** a store whose `vault.json` has `"format": 1` is read only. `layr clone
   <url>` imports its linear history into line `main` as `import` states, oldest first, with the
   Desk paper key, and writes an `import` attestation with the Desk seq and Layr state id when
   core lands. A v1 store is never written.

## Peer remotes

`ssh://host/<project>` runs `ssh host layr serve-peer <project>` (internal command, hidden from
help). `sync <remote>` = today's two-way exchange. `push <remote>` = the same exchange one way
(records and states the peer lacks); `fetch <remote>` = one way back. `pull` = `fetch`, then
for the current line: fast-forward to the newest head of the line from another machine, or
merge it (the existing merge engine; conflicts stay in the state).

## Command changes (RFC 12.4, user decisions)

- `remote add <name> <url>`, `remote list`, `remote remove <name>`, `remote trust <name>
  <machine>`; `machines` and `machines trust` are gone (`remote list` shows machines too).
- Git bridge: `layr git remote|fetch|pull|push|clone|sync`; old top-level git forms fail with a
  message that names `layr git …`.
- `sync -- <command>` → `sync <remote>`; the old form fails with the new form in the message.
- **The old whole-project copy command disappears with its name** (user decision): the
  command, its code, tests, docs and messages are removed, and its name appears nowhere. It
  becomes an unknown command with no special message. Its job is done by a `file://` or `s3://`
  store remote.
- `fsck --remote <name>` verifies segments, signatures, trees and, with `--files`, every frame;
  `log --remote <name>` reads history from the store.

## Ownership

- Workstream `remote-store`: store format v2 library (`src/store_remote/`), `file://` and
  `s3://` backends (S3 SigV4 client ported from Desk, conditional and multipart puts), keys and
  slots, push/fetch/clone of stores, `fsck --remote`, `log --remote`, `gc --remote`, Desk v1
  import. Public API used by the CLI:
  `store_remote::open(url, repo) -> Store`, `Store::push(ctx, repo, opts)`,
  `Store::fetch(ctx, repo, opts)`, `Store::clone_into(ctx, …)`, `Store::verify(…)`,
  `Store::log(…)`, `Store::gc(…)`, `Store::trust(machine)`.
- Workstream `remote-cli`: `remote add/list/remove/trust` and the remote config, URL dispatch,
  peer remotes (`push/fetch/pull/sync` over ssh), `layr git …` move, removal of `machines`,
  `serve-peer` hiding, removal of the old copy command, invariant tests (sync never deletes states on the other
  side, gc never deletes another copy's states), README.
