# Short-lived machines (CI jobs)

A CI job (GitHub Actions) holds no secret. Each job makes a fresh store, so it has a fresh machine id
and key. A broker (a trusted machine of the project, the Layr Cloud executor) checks the job's OIDC
token and the project's rules, then trusts the job's machine for a short time. The gateway gives the
job a grant that ends when that trust ends. The broker and the gateway are in Layr Cloud; this page
is the layr side.

## Commands

All of them run without a prompt; `--json` prints one object.

- `layr remote trust <remote> <machine> --public-key <hex> --expires <RFC 3339 time or duration>
  [--role reader|writer] [--overlays <name>,<name>] [--json]`. Without any of these flags the trust
  is full and has no end, as before. With one of them the default role is `reader`; `--overlays`
  names the cache overlays the machine reads (and, as a writer, writes); without it, all of them.
- `layr access grant machine:<broker> trust --max-hours <n> --role reader|writer
  --overlays <name>,<name> [--name-prefix <prefix>] [--json]` lets the broker trust other machines
  within these limits. `layr access revoke machine:<broker> trust` ends it. Not built yet.
- `layr remote untrust <remote> <machine> [--json]` ends a trust at once (a suspected leak).
- `layr save --caches --save-back <line> [--json]` saves the caches of the current line to the
  save-back line of `<line>` (`<line>.checks-<os>-<arch>`, as `checks run` does) and makes no source
  state. Not built yet: until then a job makes the line with `line new <line>.checks-<os>-<arch> <line>`
  and saves with `save --caches` there (tests/t171-ci-trust.sh).
- `LAYR_MACHINE_NAME` names a new machine (store creation); without it the name is the host name.

## Rules

1. A `machine.trust` record can carry `expires_at` (unix ms), `role` and `overlays`. A trust with any of them is
   limited; its role is `reader` unless it says `writer`. The gateway grant ends at `expires_at` too, so the job
   cannot upload a record later.
2. A record of a limited machine outside its terms stays in the log, so the sequence of its machine stays whole,
   but it has no effect anywhere: no derived table, no key, reader or mode (`Project::effective_jsonl`), no
   trusted writer (`trusted_writers`), no owner. A machine always takes its own records. `LAYR_TRACE=1` names
   each such record on import. The rules are in `crates/layr-store/src/trust.rs`.
3. A record signed at or after `expires_at` has no effect. After `expires_at` the machine is no cache reader and
   gets no slot of a new key epoch. Its old slots stay until the next epoch made for another reason: its key was
   made in the job VM and ended with it, so a new epoch would protect nothing.
4. `untrust` writes a `machine.untrust` record and makes a new project key epoch without the machine at once
   (`remote rotate --remove`). Records of the machine signed at or after it have no effect. Other machines learn
   it from the next push.
5. Full machines grow from the project creator: a trust without limits by a full machine. A limited machine
   trusts, limits and untrusts nobody, also with a backdated record. When full machines disagree about one
   machine, the narrowest limits win.
6. Roles. A `reader` makes no record that others take. A `writer` (a cache writer) makes only: in the project,
   `config` (machine-local settings) and `line.create` of save-back lines (`<line>.checks-<os>-<arch>`); in the
   cache overlays its trust names, `line.create`, `commit` and `gc` of its own overlay lines. No source state, no
   other line, no move of a line another machine made, no delete, no trust.
7. A delegation is a `trust.delegate` record of a full machine (not built yet). A `machine.trust` record signed
   by a broker counts only when, at its time, a delegation of that broker is live and the trust is inside it:
   `expires_at` at most `max-hours` after the record time, the same or a smaller role, overlays a subset, and the
   machine name starts with the prefix.
8. The gateway lets a job write only the record segments of its own machine: a clone takes every machine of the
   records it reads (`Trust::All`).

## Tests

- A job machine with `--expires 2s`: its records before the expiry import, a record after it is
  refused.
- A broker inside its delegation trusts a machine; a trust beyond `max-hours`, with a wider role,
  another overlay or a wrong prefix is refused, and so is a trust after the revoke.
- A writer job saves caches with `--save-back main`; a reader takes them. A writer record that
  makes a source state or deletes a line is refused.
- `untrust` then push: the overlays the machine could read have a new epoch.
