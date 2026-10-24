# Refs, changes, review and checks

This page is part of the [Layr documentation](../README.md#documentation).

## Refs, changes, and attestations

Refs name states without a work folder, for example `env/prod` or `pins/build`.
Mutable ref names must start with `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, or `derived/`.
`ref set` refuses line names. Use `save -m <message>`, `merge`, or `reset` to change a line.
`--expect` compares the current state before the update. A mismatch changes nothing and returns an error.
`--ttl` accepts positive durations such as `30s`, `5m`, `2h`, and `1d`.
All ref and change commands accept `--json`.

```sh
layr ref set env/prod head
layr ref set env/prod head~1 --expect head
layr rev-parse 'env/prod@{1}'
layr ref list env/ --json
layr ref log env/prod
layr undo
```

`<ref>@{0}` selects the current position. `<ref>@{1}` selects the previous position. `ref log` lists the newest position first.
`ref log` retains positions after delete. `undo` restores a ref if no later update changes it.
Garbage collection (GC) keeps live ref targets and their dependencies.
Expired refs stop resolving. GC records their removal. The normal undo window can retain earlier positions.

These revision prefixes are reserved: `env/`, `release/`, `pins/`, `holds/`, `leases/`, `caches/`, `derived/`,
`changes/`, `views/`, and `attestations/`. Line names cannot start with these prefixes.
`views/<id>` resolves a saved view to its shared state. `line new` and `try` also use its overlay stack.

Each new source state has a `change_id`. Amend, replay, and pick preserve it.
A merge state has a new identifier. Old states without `change_id` remain readable.
`changes/<id>` selects the latest version. `changes/<id>/v1` selects the first version.
`change show` lists versions and their attestations. `change log` lists their operations.
`change diff` compares each version's patch against its own parent, so unrelated base changes do not enter the interdiff.

Attestation kinds: `check`, `approval`, `origin`, `import`, `quarantine`, and `revoke`.
Each attestation is part of an immutable operation record with a machine signature.
It records the exact subject state, optional ChangeId, payload digest, signer, and time.
`--payload` reads a file as the caller and records its SHA-256 digest. It does not store the file contents.
The optional payload reference field is reserved for an object in an allowed layer.

`approve` writes an approval attestation. `approve --withdraw` appends a revoke attestation for the caller's approvals.
`attest revoke` revokes the caller's earlier attestations of the subject on this machine.
The original evidence remains in the log. A revoke cannot withdraw another caller's evidence.
`try` records a check attestation and its command result. Protected-line rules continue to use approvals and checks.
The old `approvals` and `checks` commands fail and name `layr attestations` as their replacement.
Existing signed approval and check records populate the new attestation table without changes to their signatures.
Attestations do not keep a subject alive. Their records remain available while the subject lives.

State kinds `cache` and `derived` have optional `derived_from` and `context` fields.
They have no source parents, line, or ChangeId. Lines, source diffs, merges, and Git exports reject them.
A live ref can keep an auxiliary state and its source alive.
Existing cache snapshots keep their attachment to source states and their wire format for older clients.
They remain cache data outside source diffs. This change does not convert those snapshots into separate StateIds.
The new kinds support separate cache and derived states through the store API.

The database schema version stays at 6. New tables are additive. Optional fields are absent from old state records.

## Ref protection and the merge queue

`protect` accepts ref patterns such as `env/prod` and `release/*`.
Ref rules accept `--update`, `--expect`, `--no-expect`, `--approvals`, `--approvers`, and `--check`.
They refuse `--direct`, `--no-direct`, `--rewrite`, and `--path`, which apply to lines.

A protected ref requires its update role and, by default, `--expect`.
Create a ref before you enable its default protection rule, or use `--no-expect` for its first update.
The new target needs the required approvals from allowed users other than its authors.
A delete requires evidence for the current target. A scoped declassification approval does not count as a subject approval.
Each named check needs a passing attestation for that exact state and the configured command.
`ref set` and `ref delete` run no check commands. Use `try` or the queue to produce the evidence first.

```sh
layr protect env/prod --update maintainer --approvals 1 --check unit
layr ref set env/prod topic --expect env/prod
layr queue add topic --into main
layr queue list --json
layr queue run --into main
```

The queue stores the exact source state and its ChangeId. Later source edits do not change an entry.
The default target is `line.<name>.upstream` when that setting exists, else `main`.
`queue run` processes pending entries in order. An entry becomes `done` or `failed`, with its result or reason.
A failure does not stop later entries. A conflict leaves the target head and files unchanged.
A target head change causes one retry. A second head change fails the entry.
The public `merge --expect` syntax still pins the source. The queue also checks the target head under the merge lock.
The queue uses the existing line rules, approvals, merge drivers, and checks.
A passing check for the exact result state and equal `argv` is reused.
Legacy records without `argv` are reused only for a single word without spaces. Other legacy results run again.
A caller who cannot update a target cannot process its queue.

Queue entries, removals, and results have signed records. Successful line moves and queue completion share one record.
`undo` can restore an entry or reverse a completed move and its status together.
Pending and failed entries hold their source through `holds/queue/<entry>` refs. Remove an entry to release its hold.
All queue commands accept `--json`. Queue list includes pending, done, and failed entries.

Set `queue.run` to a comma-separated list of local line names to enable automatic passes.
A new entry triggers a pass for its configured target. The daemon also runs a pass on each tick.
Automatic passes use the target line owner and the normal line policy. Configured commands run as that owner.
Without `queue.run`, only `queue run` processes entries. Queue runners on one machine use one project lock of their own.

Set `queue.probes` to `true` to run every probe on the target head and the merge result before the target moves.
The queue uses the same `try` sandbox and stored results as `probe run`.
Equal arguments and exact states reuse their results, including command failures.
`queue list` shows each probe outcome. `--json` includes the stored comparison.
The change's Behavior section reads that same comparison, including failed merge results.

A probe's `fail_over = 0.20` fails an entry when any metric or output file size grows by more than 20 %.
The limit applies independently of `noise`. A decrease or equal value passes.
A zero or absent baseline with a positive result exceeds every finite relative limit.
Without `fail_over`, a probe result never fails an entry, including a nonzero command exit.
A refusal names the probe, metric or file, both values, relative growth, and limit:

```text
probe bench-checkout: p95_ms 41 → 52 (+27 %) is over the limit +20 %
```

Probes run implicitly only in a queue with `queue.probes = true`.
Ordinary merges, diffs, and `change show` execute no probe commands.

## Probes and behavior differences

A probe is an explicit command from the exact revision's `layr.toml`.
A project can define probes without runnables.

```toml
[probe.api]
run = ["python3", "tests/probe.py"]
outputs = ["results/api.json"]
kind = "json"

[probe.performance]
run = ["./measure", "--json", "results/timing.json"]
outputs = ["results/timing.json"]
kind = "number"
noise = 0.05
fail_over = 0.20
```

`run` is a nonempty argument array.
The command starts at the tree root.
`outputs` lists relative file or folder paths without `..` or `.git`.
Output links and special files are refused.

| Kind | Comparison |
| --- | --- |
| `files` (default) | Text patches for each file. Binary files use their size and SHA-256 hash. |
| `json` | Structural changes by JSON Pointer, including array indexes. |
| `number` | Each JSON file contains an object of numeric metrics. Changes use `(head - base) / abs(base)`. |

The number kind ignores changes whose absolute relative value is at most `noise` (default `0.05`).
A zero baseline has no relative ratio; a new nonzero value still appears.
Files with JSON output must fit the existing text size limit.

```sh
layr probe run main --probe api
layr probe run changes/CHANGE_ID --json
layr probe diff main feature --json
layr change show CHANGE_ID
```

`probe run` executes commands. The queue also runs probes when `queue.probes` is `true`.
A diff or `change show` reads stored results and reports `not run` when a result is absent.
Outside the queue, Behavior compares stored results for the newest version and its merge base against the upstream, or `main`.
It does not select a view implicitly.
If only view results exist, it lists their IDs and shows the explicit `probe diff views/<a> views/<b>` command.

Probe commands are code from the revision's author.
They use the existing `try` copy and run as the caller with a clean environment.
The command's standard output goes to standard error, which keeps JSON command output valid.
The caller's working files do not change.
Probe outputs from a view require every project reader to have permanent read access to each overlay.
A private overlay causes refusal, with its name in the error.
This uses the existing `try` isolation; it does not add a network or system-call sandbox.

Each result has a check attestation with the probe name, exact arguments, exit code, and output state.
The output state has kind `derived`, context `probe-v1/<name>`, and `derived_from` set to the exact subject.
The ref `derived/probe-v1/<name>/<subject>` retains this output state through garbage collection.
A ViewId in `derived_from` records provenance and does not retain the view's source states.
The diff can use the definitions and output states from stored checks after source collection.
It reports `source collected` when source files are no longer available.
A missing result cannot run again without its source files.
The same subject and arguments reuse a stored result, including a failed result, while its attestation and outputs remain available.
A probe command failure makes `probe run` return a nonzero status.

## Views

A view identifies a shared state and an ordered stack of overlay states.
Its ViewId is the full SHA-256 digest of this canonical JSON, without spaces:

```json
{"format":1,"shared":"<state>","layers":[{"overlay":"<name>","state":"<state>"}]}
```

Field order follows the example. Layer order follows the stack.
Access policy stays outside the identity. Changes to reader rights do not change the ViewId.
A state without overlays has an empty `layers` array.
Working edits do not change the descriptor until a state changes.

```sh
layr view main --json
layr view save main --json
layr view show <id> --json
layr line new reproduce views/<id>
layr try views/<id> -- ./test.sh
layr attest approval views/<id>
layr attestations views/<id> --kind check --json
```

`view save` writes a signed `view.save` record. Repeated saves of the same descriptor write no extra record.
`views/<id>` resolves after that record reaches the local machine.
Commands that need a state use the shared state. `line new` and `try` use the complete stack.
`try main` and `try head` also use the line's current stack.
An explicit state revision has no layers.

A line from a view preserves its exact layer states, including older states of shared-mode overlays.
It initializes missing overlay lines. It does not move an existing overlay line.
A later private save can fail the existing stack check if that overlay line has advanced.
Use `layr overlay use` to take its newest state before the named save.

The caller must read every layer before `line new` or `try` can create a view.
A different line owner must also read every layer. A refusal names the overlay.
Descriptors contain overlay names and state ids. Project readers may read descriptors without access to the private files.
A descriptor can travel to another machine while its private layers stay local.

Try checks with layers name the view as their attestation subject.
Checks without layers retain the state subject. Check payloads include the exact argument array.
`LAYR_TRY_STATE` names the shared state. `LAYR_TRY_VIEW` also names the view when it has layers.
A check of one view does not certify a different stack with the same shared state.

Saved descriptors and attestations do not keep states from garbage collection.
`view show` can still print a descriptor after a state is collected.
Full-view resolution reports `view <id> refers to a collected state <state>`.
Before collection, pin the shared state with `layr ref set pins/<name> views/<id>`.
Overlay states stay through their own live line history.

## Find the first bad state

```sh
layr bisect run --good head~10 --bad head -- sh -c 'make test'
layr bisect run --auto --good head --json -- sh -c 'make test'
layr bisect run --ref-log env/prod -- sh -c 'make test'
```

Bisect searches the first-parent history between the good state and the bad state.
The command must pass before the fault and fail after the fault.
Each test uses `try` and writes a check attestation.
A check with the same command and state is reused, including a failed check.
The working folder does not change.

Without `--bad`, bisect saves the current working content as an auto state.
Without `--good`, bisect uses the newest ancestor with a passing check for the same command.
Use `--good` when no such check exists.
`--auto` includes the line's auto states between the bounds, in time order.
The result includes the first bad state, its message, its kind, and origin payload digests.
`--json` includes every test step and whether bisect reused its result.
Test output goes to standard error with `--json`.

`--ref-log` searches the positions of that ref.
Its default bad state is the newest ref position.
Its default good state is the newest older position with a passing check for the command.
Explicit bounds must be positions of that ref.

## Secret checks

Layr checks added lines before a shared named save or merge and before an export or push.
It uses exact values from overlays that the caller can read and from files that match `secrets.paths`.

```sh
layr config secrets.paths '[".env", "private/*.key"]'
```

The sources include unsaved files in the caller's lines.
Candidates include `KEY=VALUE` values and whole files of at most 4 KiB.
Each candidate needs at least 12 characters.
Values with only letters or only digits need at least 20 characters.
The index contains only HMAC-SHA256 values (a keyed hash).
The random key stays in the project's private `meta` folder.
Each check rebuilds the index to include source changes.
For changed files above 64 MiB, Layr checks the whole new file as a stream.
These refusals give a byte offset instead of a line.

A refusal gives the path, line, and a declassify id.
It never prints the value.
An admin can permit that value at that path:

```sh
layr approve --declassify 'src/config.rs@<id-from-refusal>' head
```

This writes an approval attestation with the full hash and path.
The permission applies to that value at that path throughout the project.
A different value or path needs another approval.
`layr approve --withdraw <approval-subject>` revokes the caller's approvals on that subject and restores the check.

Encoded or split secrets are not detected.
This check adds protection to private overlays and does not replace them.

