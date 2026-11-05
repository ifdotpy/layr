# Short-lived machines (CI jobs)

A CI job (GitHub Actions) holds no secret. Each job makes a fresh store, so it has a fresh machine id
and key. A broker (a trusted machine of the project, the Layr Cloud executor) checks the job's OIDC
token and the project's rules, then trusts the job's machine for a short time. The gateway gives the
job a grant that ends when that trust ends. The broker and the gateway are in Layr Cloud; this page
is the layr side.

## Commands

All of them run without a prompt; `--json` prints one object.

- `layr remote trust <remote> <machine> --public-key <hex> --expires <RFC 3339 time or duration>
  [--role reader|writer] [--overlays <name>,<name>] [--json]`. Without `--expires` the trust has no
  end, as before. The default role is `reader`.
- `layr access grant machine:<broker> trust --max-hours <n> --role reader|writer
  --overlays <name>,<name> [--name-prefix <prefix>] [--json]` lets the broker trust other machines
  within these limits. `layr access revoke machine:<broker> trust` ends it.
- `layr remote untrust <remote> <machine> [--json]` ends a trust at once (a suspected leak).
- `layr save --caches --save-back <line> [--json]` saves the caches of the current line to the
  save-back line of `<line>` (`<line>.checks-<os>-<arch>`, as `checks run` does) and makes no source
  state.
- `LAYR_MACHINE_NAME` names a new machine (store creation); without it the name is the host name.

## Rules

1. A `machine.trust` record can carry `expires_at`, `role` and `overlays`. A record of that machine
   signed after `expires_at` is refused on import (the signer's record time). The gateway grant ends
   at `expires_at` too, so the job cannot upload a record later.
2. After `expires_at` the machine gets no new key slots. Its old slots stay until the next key epoch
   made for another reason: its key was made in the job VM and ended with it, so a new epoch would
   protect nothing.
3. `untrust` writes a `machine.untrust` record. Records of the machine signed after it are refused,
   and every overlay that the machine could read gets a new key epoch at the next push.
4. A delegation is a `trust.delegate` record of a trusted machine that is not short-lived. A
   `machine.trust` record signed by a broker counts only when, at its time, a delegation of that
   broker is live and the trust is inside it: `expires_at` at most `max-hours` after the record time,
   the same or a smaller role, overlays a subset, and the machine name starts with the prefix. A
   short-lived machine can never trust another machine.
5. Roles. A `reader` makes no record that others take. A `writer` (a cache writer) makes only:
   cache states and lines in the cache overlays it names, and in the project the `line.create` and
   save records of save-back lines (`<line>.checks-<os>-<arch>`) whose cache states name a source
   state that the store has. It makes no source state, no other line, and no delete. A record outside
   its role is refused on import.

## Tests

- A job machine with `--expires 2s`: its records before the expiry import, a record after it is
  refused.
- A broker inside its delegation trusts a machine; a trust beyond `max-hours`, with a wider role,
  another overlay or a wrong prefix is refused, and so is a trust after the revoke.
- A writer job saves caches with `--save-back main`; a reader takes them. A writer record that
  makes a source state or deletes a line is refused.
- `untrust` then push: the overlays the machine could read have a new epoch.
