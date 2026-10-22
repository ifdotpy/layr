# Store format v2

The decisions in `remote-design.md` define this format. Desk v1 stores remain read only.

## Objects

- `store.json`: `{"format":2,"project":"<UUID>","recipient":"age1..."}`.
- `slots/paper.age`: the vault identity, encrypted to a paper identity.
- `slots/machine-<UUID>.age`: the vault identity, encrypted to the existing machine key as an age `ssh-ed25519` recipient.
- `records/<machine UUID>/<first>-<last>.age`: exact signed JSONL records. Sequence numbers have 20 decimal digits.
- `states/<state UUID>/<attempt>.tree.age`: a header line and one zstd frame, encrypted with age.
- `states/<state UUID>/<attempt>.<part>.data.age`: concatenated zstd frames, encrypted with age. Parts have four decimal digits.

An attempt has eight lowercase hexadecimal digits from the operating system random source.
The first complete attempt in name order represents a state.
A tree object is the commit point. All its data parts must exist first.

The part size target is 16 MiB. A larger frame has a separate part, same as Desk v1.
S3 objects above 256 MiB use multipart upload. Multipart completion uses `If-None-Match: *`.
All other object writes also use `If-None-Match: *` or atomic exclusive publication for a folder.
No object is replaced. There are no mutable store refs.

## Encoding and signatures

Canonical JSON sorts object keys by Unicode code point. It uses no spaces or fractional numbers.
Strings use Python `ensure_ascii` escapes, including lowercase UTF-16 surrogate pairs.
For example, `{"z":"é","a":1}` becomes `{"a":1,"z":"\u00e9"}`.

A tree is `{"files":{"<path>":["<mode>",size,"<SHA-256>",[links]]}}`.
Modes are `100644`, `100755`, and `120000`.
A symlink stores its target bytes as file content.
A link is `["<state UUID>","<attempt>",part,offset,length,"full"|"delta"]`.
Offsets address decrypted data parts. A chain starts with the newest frame and ends with one full frame.
A chain permits at most 16 delta frames. A tree chain permits at most 31 delta trees before a full tree.
A delta uses the previous plaintext as a raw zstd prefix. The compression level is 3.
A file delta requires both versions to be at most 32 MiB and must be smaller than its full frame.
Content with an existing SHA-256 reuses that chain across lines.

The tree header fields are `v`, `state`, `attempt`, `tree`, `base`, `parts`, `machine`, and `sig`.
`v` is 2. `tree` is `full` or `delta`. `base` is null or `["<state UUID>","<attempt>"]`.
`sig` is the standard Base64 Ed25519 signature with padding.
The signed bytes are:

```text
UTF8("layr/store/v2/tree") || 0x00 || canonical(header without sig) || 0x0a || canonical(tree)
```

The machine key comes from its first signed record, as in peer replication.
Records retain their existing signatures over their exact record bytes. Segments do not rewrite records.
New record fields, refs, and attestations travel inside these records without a separate store format.
Readers validate sequence continuity, project identity, signatures, and local trust before they import records.
Readers validate tree signatures and each restored file's size and SHA-256.

State IDs are existing canonical UUIDs. They are not ciphertext hashes.
The design removes RFC HMAC object names, HKDF naming keys, manifests, and CAS refs.
The signed record log carries parents, changes, refs, and attestations.
The immutable tree header authenticates each state representation.

## Keys and recovery

The vault identity is an age X25519 identity. `store.json` contains only its recipient.
A paper key contains the paper identity's 32-byte secret as 24 BIP39 words, as in Desk v1.
A machine slot uses the machine's existing Ed25519 seed through the standard OpenSSH encoding.
The age and ssh-key crates perform the key conversion and encryption.
A machine without a slot can encrypt uploads to the public recipient. It cannot decrypt the store.

## Retry and collection

A record segment keeps its name and signed plaintext across retries.
A 412 means the object already exists. Readers verify its record signatures and reject a conflicting sequence.
A state retry uses a new attempt. A lost response can leave another complete attempt with the same state ID.
No retry replaces an object. An incomplete attempt does not become visible as a state.
The local index records only attempts whose tree publication completed.

Collection removes only data parts of incomplete attempts older than 24 hours.
Collection checks again for the attempt's tree immediately before deletion.
Writers refresh their attempt if an upload takes 24 hours. Complete attempts and their delta bases remain.
The store keeps all record segments and complete states by default.

## Errors

| Condition | Result |
|---|---|
| Unsupported store or tree version | Stop before an import or write |
| Desk v1 write | Reject; import into a new local project |
| No readable slot | Reject reads; allow an upload with the public recipient |
| Record sequence gap or conflicting bytes | Reject the segment set |
| Invalid record or tree signature | Reject the object |
| Wrong project ID | Reject before importing records |
| Unsafe path, frame range, or excessive chain | Reject the tree |
| File size or SHA-256 mismatch | Reject the restore; do not publish the snapshot |
| Conditional object already exists | Preserve the stored object |
| Uncertain upload response | Return the error; retry with the rules above |
| Malformed S3 multipart completion | Return the error and abort the multipart upload |

## Test vectors

`canonical::tests::equals_python_json_dumps` contains the complete Unicode encoding vector.
`format::tests::signatures_bind_header_tree_and_domain` uses Ed25519 seed `07` repeated 32 times.
The signed tree is `{"files":{}}`. Changes to its header or content must fail verification.
`keys::tests::paper_and_machine_slots_open_the_same_identity` checks both slot forms and a wrong machine key.
`zst::tests::frames_decode_with_and_without_a_base` checks full and raw-prefix delta frames.
