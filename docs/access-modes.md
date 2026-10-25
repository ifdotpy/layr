# Project access modes and epochs

Projects and overlays use `private`, `service`, or `public` access. These modes use
ordinary age reader slots. `private` is stricter than `service`, which is stricter
than `public`.

```sh
layr remote mode archive private
layr config service.recipient age1...
layr remote mode archive service
layr remote mode archive public
layr remote rotate archive --remove machine:<UUID> --remove person:ann@example.com --reason 'Reader removed'
layr overlay mode settings private
```

A project admin on a trusted writer machine can change a remote's mode or rotate
its key. `remote.<remote>.service-recipient` can override `service.recipient` for
the project slot. An overlay in service mode uses `service.recipient`.
Opening access adds a slot to the existing epoch and opens all existing data to
the new audience. Closing access rotates the project key. `remote public <remote>
off` closes to private through the same rotation. Earlier data remains readable
to anyone who already had access. A removed reader cannot decrypt new epochs.

Each overlay defaults to private, including overlays created in public projects.
An explicit `overlay mode` can open a visible overlay only as far as the project
mode. Hidden overlays always remain private. Closing the project also tightens
more open overlays and lists them. It requires the overlay key manager's authority.
Overlay mode changes take effect on the next push; project closing pushes its
overlay tightening before publishing the new project epoch.

The reader set retains machine, person, and paper recipients. Old person grants
that lack the public recipient must be repeated before rotation. If an old store
pin lacks the paper recipient, set `LAYR_PAPER_KEY_FILE` once. Neither operation
writes a secret identity into signed records or command output.

## Store contract

The root `store.json` and its recipient stay immutable. They are epoch `root`
(epoch zero), and remain the local and person-index trust anchor. Each new epoch
uses these relative object paths:

| Object | Pattern |
| --- | --- |
| Signed discovery commit | `keys/<epoch UUID>-<record UUID>.json` |
| Epoch descriptor | `project/epochs/<epoch>/store.json` |
| Reader slots | `project/epochs/<epoch>/slots/{machine-<UUID>,person-<login>-<fingerprint>,paper,service,public}.age` |
| Public opener | `project/epochs/<epoch>/public.json` |
| Sealed earlier identity | `project/epochs/<epoch>/history/{root,<earlier epoch UUID>}.age` |
| Signed encrypted logs | `project/epochs/<epoch>/records/<machine>/<first>-<last>.age` |
| Encrypted states and index | `project/epochs/<epoch>/{states,index}/...` (existing v2 names) |
| Person requests and executor claims | `project/epochs/<epoch>/{requests,claims}/<id>.json` |

The discovery object contains the exact signed `project.key` record. Its data
contains only `project`, `parent`, `parent_recipient`, `epoch`, `recipient`, `mode`,
and `readers_hash`. The full reader map is in a signed `project.readers` record
inside the encrypted epoch log. Its canonical SHA-256 is `readers_hash`.
A signed `project.mode` records mode, epoch, and optional reason. The model exposes
access mode and numbered epochs without key material.

Every earlier epoch identity is sealed directly to the new recipient with the
existing `seal_slot`. The new epoch has the complete signed project log. Earlier
states keep their original names, ciphertext, and identities. Readers resolve
state objects across the current namespace and sealed history. They verify every
historical identity against the recipient lineage, exact root history, signed
discovery records, and the encrypted reader hash.

The discovery commit is published last, after the new slots, sealed identities,
complete log, and tightened overlays are present. A retry uses its pending signed
record and a local machine-sealed identity. Conflicting epoch children are
ambiguous and refuse reads and writes. Current clients discover the latest epoch
before writing; an old executable with raw bucket write access cannot be
revoked by changing reader slots. Storage authorization must also restrict writers.

Overlay namespaces stay at `overlays/<overlay>/...`, bound to the immutable root
recipient. Private overlay slots and hidden metadata are independent of project
openness. Explicit public overlay slots use their own identity and signed
manifest; they never reuse the project vault identity.

## Gateway integration

The native client extends the existing gateway proof exchange with an optional
`epoch=<UUID>` query on `slots`, `challenge`, and `token`. The slots response and
challenge/token bodies keep their existing formats. Direct person-slot GET paths
include the epoch namespace and retain the login check. A current-epoch proof can
read earlier ciphertext and requests for history. An earlier proof must not read
new epoch objects. Changing epochs clears the cached read proof.

Epoch discovery and descriptors are metadata available before a slot opens.
Anonymous readers discover the latest epoch before requesting its public opener;
an old root `public.json` never authorizes a later private epoch. A gateway must
allow current public records, states, sealed history and discovery through its
read-only public routes, and enforce the audience of each explicitly public overlay.
The epoch-aware server deployment is coordinated separately from this native
change. A server that lacks epoch routes fails closed for rotated stores.
