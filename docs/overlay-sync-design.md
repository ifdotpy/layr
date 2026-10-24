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

## Key managers

The creator machine, which signs `overlay.create`, is the first key manager.
A manager sets the manager machines with `layr overlay managers <name> machine:<id>...`.
A remaining manager must sign removal of another manager.
Only a key manager can sign `overlay.key` or change the readers record.
Other trusted machines can push data to the published recipient.
Readers refuse unauthorized key and readers records and name their signer.

Signed `overlay.managers` records remain in the overlay log.
The parent project carries each exact record inside an `overlay.authority` record.
This proof lets a reader verify key authority before an epoch slot opens.
A manager transition retains hashes of previously authorized key and readers records.
Those exact historical records remain valid after their manager is removed.
If every manager machine is lost, create a new overlay and move the files.
Use two manager machines.

## Hidden overlays (decided 2026-10-24)

Two levels of privacy:

- **Private** (today's default): everybody who reads the project sees that the overlay exists,
  its name, readers and routed patterns; only readers get its files.
- **Hidden** (`layr overlay create <name> --hidden`): people and machines that are not readers do
  not learn that the overlay exists. No list, count, name, reader, pattern or operation of it is
  visible to them.

Rules for a hidden overlay:

1. Nothing about it is in the parent project's shared records or config: no `overlay.<name>`
   setting, no routed patterns, no readers, no `overlay.authority` record, no operation in the
   parent's op log. All of these live in the overlay's own records, which only readers can open.
2. Parent states do not name its layers. A reader's machine builds the line view from the
   overlay's own records ("follow" mode), so shared states are identical for readers and
   non-readers.
3. Readers find it through their key: the overlay's records hold the parent project id, and a
   reader's machine or person slot opens them. A new reader gets a slot at the next push and then
   discovers the overlay.
4. Stores still hold its encrypted objects under `overlays/<random id>/`. Someone with store access
   can see that encrypted overlay objects exist and their sizes, never names or contents. The
   documentation says so.
5. Peers receive it only when the peer is allowed (`overlay.<name>.peers`, stored in the overlay).
6. The secret detector of a non-reader does not know its values; a value that only a hidden
   overlay holds is caught on readers' machines (their saves and pushes), not on others.
7. A visible overlay cannot become hidden (its name is already known); a hidden one can become
   visible (`layr overlay show-to-project <name>`).

UI: a hidden overlay appears only to its readers, with a "hidden" badge; non-readers see no row,
no count and no badge on files.
