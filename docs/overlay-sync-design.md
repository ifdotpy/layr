# Private overlays in pushes and sync (lead)

Status: decided 2026-10-23 (user decision: private overlays must travel; who sees them is the
user's own access setting). Overrides `docs/remote-design.md` decision 4 ("files of private
overlays are never sent").

## Rule

Every push and sync carries every overlay of the project. Visibility follows the overlay's
readers (`layr overlay readers`), as it does on one machine today. Nobody without read access
gets plaintext, on any machine or in any store.

## Peer remotes (`ssh://`)

The peer is a Layr service that enforces the same signed access rules. Peer sync sends overlay
records and states like project records and states (overlays are internal projects
`<project>%<overlay>` with their own records). The receiving service applies the overlay's
readers exactly as the sending service does. No new keys.

Limit: the root user of a peer machine can read anything stored on its disk; Layr's access rules
bind users of the service, not root. So a peer gets an overlay only if the overlay allows it:
the overlay setting `overlay.<name>.peers` lists the machines that may hold it (default: the
machines of the overlay's readers that this machine trusts). Other peers get the shared files and
the report says which overlay stayed local. Store remotes have no such limit, because their
overlay objects are encrypted to the overlay's readers.

## Store remotes (`file://`, `s3://`, `layr+https://`): one key per overlay

- Each overlay has its own age identity per store remote (a **layer key**), separate from the
  store's vault identity. Shared files stay encrypted to the vault recipient, as now.
- The layer recipient is published in a signed record of the overlay (`overlay.key {overlay,
  epoch, recipient}`), never only in a plain object, so a store or gateway cannot swap it
  (same reason as the store recipient pin).
- Objects: `overlays/<overlay id>/records/…` and `overlays/<overlay id>/states/…`, same format
  v2, encrypted to the layer recipient. Tree signatures as for project states.
- Slots: `overlays/<overlay id>/slots/<epoch>/machine-<id>.age`, `…/person-<login>.age`, and a
  paper slot for the overlay owner. A slot exists only for principals that are readers of the
  overlay.
- `push` adds missing slots for current readers whose public keys it knows (machines from the
  trust records, persons from their recipients), before uploading overlay objects.
- Removing a reader starts a new epoch: a new layer identity, new objects encrypted to the new
  recipient, slots for the remaining readers only. Data already copied by the removed reader
  stays readable to them (RFC 7.3); the README says so.
- `fetch`/`clone` import an overlay only with a slot that opens; otherwise the report says
  `overlay <name>: no key on this machine` and the shared files still import.
- A write-only machine (no slot) can still push overlay objects: it encrypts to the published
  layer recipient.
- `layr mount` shows overlay files to a reader with a slot, merged into the line view as the
  working folder shows them.

## Secret detector

Unchanged: it still refuses a secret value in shared files. Secrets inside a private overlay are
allowed, travel encrypted to that overlay's readers, and are the intended place for them.

## Tests

Two machines and one store: A owns overlay `secrets` with reader B. A pushes; B fetches and sees
the files; C (project reader, not overlay reader) fetches and sees the shared files only, and no
object under `overlays/` opens with C's keys. Remove B as reader, A pushes again: B cannot open
the new epoch. Peer sync A→B carries the overlay; A→C carries it, and C's service refuses to
show it.
