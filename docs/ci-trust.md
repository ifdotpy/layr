# Short-lived machines (CI jobs)

A CI job (GitHub Actions) holds no secret. Each job makes a fresh store, so it has a fresh machine id
and key. A broker (a trusted machine of the project, the Layr Cloud executor) checks the job's OIDC
token and the project's rules, then trusts the job's machine for a short time. The gateway gives the
job a grant that ends when that trust ends. The broker and the gateway are in Layr Cloud; this page
is the layr side.

## Commands

All of them run without a prompt; `--json` prints one object.

- `layr remote trust <remote> <machine> --public-key <hex> --expires <RFC 3339 time or duration>
  [--role reader|writer|merge] [--overlays <name>,<name>] [--json]`. Without any of these flags the trust
  is full and has no end, as before. With one of them the default role is `reader`; `--overlays`
  names the cache overlays the machine reads (and, as a writer, writes); without it, all of them.
- `layr access grant machine:<broker> trust --max-hours <n> --role reader|writer
  --overlays <name>,<name> [--name-prefix <prefix>] [--json]` lets the broker trust other machines
  within these limits. `layr access revoke machine:<broker> trust` ends it. The broker must be a
  trusted machine; it becomes a broker for good: a trust it makes counts only inside its newest
  delegation made before that trust. Make the broker a key manager of the cache overlays
  (`layr overlay managers <overlay> machine:<owner> machine:<broker>`), so its push gives a new job
  the caches.
- Per job, the broker runs `layr remote trust <remote> <job id> --public-key <hex> --name <job name>
  --expires 1h --role writer --overlays <names> --json`, then `layr push <remote> --caches`. The job
  gets its id, key and name from `LAYR_MACHINE_NAME=ci-… layr remote self --json`. A trust outside
  the delegation is refused with exit 128.
- `layr remote untrust <remote> <machine> [--json]` ends a trust at once (a suspected leak).
- `layr save --glasses --save-back <line> [--json]` saves the caches of the current line to the
  save-back line of `<line>` (`<line>.checks-<os>-<arch>`, as `checks run` does) and makes no source
  state (docs/checks.md).
- Owner decision 2026-11-11: a cache is an ordinary overlay, so `--caches` and `save --caches`
  go away (docs/cache-overlays.md section 2). The commands above are what is built.
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
   machine, the narrowest limits win. A trust, a delegation or an untrust counts only when its signer was not
   untrusted at or before its time. The untrusts count in their time order: each one needs a signer that is
   full with the untrusts before it.
6. Roles. A `reader` makes no record that others take. A `writer` (a cache writer) makes only: in the project,
   `config` (machine-local settings) and `line.create` of save-back lines (`<line>.checks-<os>-<arch>`); in the
   cache overlays its trust names, `line.create`, `commit` and `gc` of its own overlay lines. No source state, no
   other line, no move of a line another machine made, no delete, no trust. The role covers the whole record,
   because a record of any op can carry states and line moves: a writer's `config` makes no state and moves no
   line, its `line.create` makes only the one new save-back line that it names, and its overlay `commit` makes
   states only on the lines it moves. No record of a limited machine (its `machine` record too) carries a queue
   change, a ref change or an attestation, and its `machine` record makes no state and moves no line. The
   `config` of a writer cannot change project settings: other machines keep it as `<key>@<machine>`.
   The creator can give other machines its own rights: `layr config admin.machines '["<machine id>", ...]'`.
   The records of those machines then count as the creator's (project settings, line heads, machine
   approvals, main heads for checks), so two machines of one owner work on a project alike. Only the
   creator's own latest `admin.machines` counts: an admin machine names no further admin.
7. A delegation is a `trust.delegate` record of a full machine. A `machine.trust` record signed by a broker
   counts only when, at its time, a delegation of that broker is live and the trust is inside it: `expires_at`
   at most `max-hours` after the record time, the same or a smaller role, overlays a subset, and the machine name
   (`name` in the trust record) starts with the prefix. A broker's trust without an end never counts.
8. The gateway lets a job write only the record segments of its own machine: a clone takes every machine of the
   records it reads (`Trust::All`).

## Merge machines

A queue merge executor (the Layr Cloud merge job) needs to publish merges and releases, but not to change
project policy. Give it the `merge` role: `layr remote trust <remote> <machine> --role merge`, and name it with
`layr config queue.merge-machine <machine>`. The command also narrows a machine that is already fully trusted:
it writes a second trust record, and the narrowest limits win (rule 5). Push the record.

A record of a merge machine counts only when it is one of these, and only in the project log. Each check reads
the reader's own state, never the `before` values that the record claims. Import uses the derived tables.
Release verification replays accepted full and merge records in time, machine and sequence order:


- `queue.result` that ends an entry the reader has as pending (`done` or `failed`), with the entry's other fields
  unchanged. It may release that entry's hold (`holds/queue/<entry>`). A `done` result may move the entry's
  target line to the result. The line keeps its name and its home machine, or becomes the merge machine's line,
  as any machine that makes a working folder of a line adopts it. Its owner stays, or becomes root (a queue runner
  as root): a root-owned line gives no user a right, and any other uid would give that uid the line elsewhere;
- `merge.candidate`, which makes states and moves no line;
- `attest` with the evidence of a person-signed request (`request` and `person_key`); readers check it against
  the person key in the project;
- `ref.set` of a `release/` ref that the reader does not have, without an end. A set of the target the reader
  has is a retry. A merge machine never moves or deletes a release ref;
- `release.files` for a release ref that the merge machine itself set, at the list's state. An attach replaces
  the list, so the files of a release that another machine made stay out of its reach;
- `request.refused`, `config`, `push` and `machine`, which make no state and move no line.

Any other record of a merge machine has no effect: it makes, renames or deletes no line, changes no other ref or
line, writes no overlay and carries no queue change or attestation outside these records. Its `config` is
namespaced as for any other machine. It trusts nobody. A broker cannot hand out the role (`access grant ...
trust --role merge` is refused); only a full machine's own trust makes a merge machine, and only of a machine
without limits. After an untrust or the end of its trust the machine is no release writer.

A merge machine's record signed more than ten minutes after the reader's clock has no effect, and a merge
result carries no overlay stack. A retry by the same machine keeps its ref authority. A same-target set by another machine removes sole
setter authority for that target run. Further retries do not restore it. A reader gives no local right to the owner of a
line whose home is another machine: that owner is a user of the other machine.

Release verification keeps queue fields as signed JSON. It updates line metadata after each accepted record.
It checks ref expiry at that record time; the future-record limit uses the current clock. A later full-writer
queue removal or ref change does not erase an earlier accepted merge or release file list.

Known limit: an import checks a merge record against the state when it arrives, and an index rebuild checks it in
time order. When records arrive out of time order, two readers can disagree about a merge record (for example a
queue result that arrives after a later removal of its entry). A merge machine signs its own record times, so an
old time is part of this limit. The file list rule therefore needs every record of the newest run that set the
release ref's target to be of the merge machine; a retry beside another machine's set gives no right.

A root-owned target line has two effects on machines other than its home, as with a root merge job of full
trust before this role: a non-root user there cannot merge into the line (`authorize_line` refuses every non-root
user on a root-owned line), and the overlay checks of the line (`project_reads`, `readable_layers`,
`check_stack`) use root's read rights, so the hidden overlay lines named for owner 0 belong to its stack. Users
still see only the overlays they read.

Known limit: a full machine that adopts a line can still give its owner uid, which is a user of that machine, line
rights on other machines through `authorize_line`. This is older than the merge role.

Owner checks (`fsck`) and the key reader lists read the records outside an ordered replay. There a merge
machine's record is checked by its shape only, which accepts at least what the replay accepts, so these lists
never lose a state that the replay keeps.

`release verify-main`, `release attach` and `release publish-public` take a merge machine as a writer: the
release ref and the queue completion of the merge machine count when its terms accept the record, and the
release state must be in the accepted main history.

Clients older than this role read `"role":"merge"` as an unknown role and take the machine as full. Upgrade every
machine that verifies releases before it relies on the limits.

## Tests

- A job machine with `--expires 2s`: its records before the expiry import, a record after it is
  refused.
- A broker inside its delegation trusts a machine; a trust beyond `max-hours`, with a wider role,
  another overlay or a wrong prefix is refused, and so is a trust after the revoke.
- A writer job saves caches with `--save-back main`; a reader takes them. A writer record that
  makes a source state or deletes a line is refused.
- `untrust` then push: the overlays the machine could read have a new epoch.
- A full machine narrowed to `merge`: its release ref arrives, its other ref and its new line do not, and a
  broker cannot delegate the role (`tests/t171-ci-trust.sh`). The Cloud V1 interoperability test publishes a
  checked release through a merge machine without full trust.
