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
`change show` lists versions and their attestations.
Each JSON version also includes `checks`: active signed command results, including
failed results, from the same attestations. Read `exit` and `argv` for that exact
version; an empty array does not mean that checks passed. Revoked checks stay in
`attestations` but do not appear in `checks`.
Its JSON includes an `operation` for each new amend, replay, or pick version.
The operation gives the actor, machine, time, kind, source state, target line, and target head.
Versions from older records omit this field. `change log` lists their operations.
`change diff` compares each version's patch against its own parent, so unrelated base changes do not enter the interdiff.

### Change numbers, titles and closing

Each change has a number, for example `#12`. `change show #12`, `change show 12` and `changes/#12` select it.
A bare number that is also the start of a change id is ambiguous: use `#12` or more of the id.
JSON output of `change list` and `change show` has these fields:

| Field | Value |
|-------|-------|
| `number` | The change number, or null when the change has no number yet. |
| `title` | The title from `change edit`. Without one, the first line of the newest version's message. |
| `description` | The description from `change edit`. Without one, the rest of that message, or null. |
| `status` | `open`, `closed` or `merged`. |
| `closed` | Only for a closed change: `actor`, `machine`, `time` and `reason`. |

How numbers work:

- Records are written offline on many machines, so a counter on each machine would give the same number twice.
  One machine gives all numbers: the numbering machine. At first it is the machine that created the project.
- The numbering machine gives numbers when a `change` command, `queue add`, `queue candidate` or `queue run` runs on
  it. It numbers every change that has none, the oldest first version first, in one signed `change.number` record.
  `queue run` and `queue candidate` give the numbers before they merge. When no change needs a number, no record is
  written.
- Another machine shows a number when it has that record, for example after `layr fetch`. Before that, the number
  is null. A new change on a laptop gets its number when the numbering machine sees the change.
- A number never changes. The first number of a change stays. A number that another change has already is ignored.
  Only the records of the numbering machine count.
- `change numbering` shows the numbering machine and the next number. On the numbering machine, an admin can give
  the role to another machine: `change numbering --hand-to <machine>`. `<machine>` is a machine name, or the id of a
  trusted machine. The record gives the new machine the next number. Numbers that the old machine writes after the
  handoff count no more. Use this before you retire the numbering machine, or to give the role to the Layr Cloud
  merge machine.
- An admin machine of the project (the creator, and the machines that the creator's `admin.machines` setting names)
  can take the role from any holder: run `change numbering --hand-to <machine>` on the admin machine. Use the id of
  the admin machine itself to take the role back. The new holder gives numbers from the admin machine's next number.
  The numbers that the old holder gives from that number on count no more, so fetch the old holder's records first.
- Only a full machine or a merge machine (docs/ci-trust.md, "Merge machines") can hold the role. `--hand-to` refuses
  a machine that is not trusted, a machine with the reader or writer role, and a machine whose trust ends: other
  machines would not take its numbers. A merge machine cannot hand the role on; an admin machine takes it.
- A number record gives at most as many numbers above the highest number as it has entries. A number above that
  limit counts for nothing.
- The usual numbering machine of a project with Layr Cloud is its merge machine. The merge machine runs
  `queue run` on each pass, so each pass numbers the new changes. No other CI step is necessary.
  To give it the role, on the project creator (or another admin machine), as an admin:

  1. Trust the merge machine with the merge role, if it is not trusted yet (docs/ci-trust.md, "Merge machines").
  2. Run `layr fetch <remote>`, so that this machine has the newest numbers.
  3. Run `layr change numbering --hand-to <merge machine id>`.
  4. Run `layr push <remote>` to the store that the merge machine fetches.

  After its next fetch and pass, the merge machine gives the numbers. Other machines show them after a fetch.
  `layr change numbering` on any machine names the merge machine.
- When the trust of the numbering machine ends (`remote untrust`), its later numbers count no more. An admin machine
  then takes the role, as above.
- We chose this rule over a number from a sort of the change records (time, machine, sequence). A sort gives a number
  at once on every machine, but a change that arrives late with an old time would move the numbers of later changes.

`change edit <change> --title <title> [--description <text> | --description-file <file>]` sets the title or the
description. An edit changes only the fields that it names. An empty value clears the field, and the change then
shows its message again. A title is one line of at most 300 characters. A description has at most 64 KiB.
`--description-file -` reads standard input.

`change close <change> [-m <reason>]` closes a change. `change reopen <change>` opens it again.
`change list` does not show closed changes. `change list --closed` shows only closed changes, and
`change list --all` shows all changes. A merged change cannot be closed or reopened, and a merged change shows
`merged` also when a close record arrives later. A change is merged when its newest version is in the history of a
`main` line or when a queue entry of that version is done.

Who can edit, close or reopen a change: the author (the user who made the first version, on that machine) or a
maintainer of the project (role `line.manage`). The machine that writes the record checks this. Other machines
accept the record like other records of a trusted machine.
`layr undo` skips the change records (edit, close, reopen, and the numbers that the numbering machine writes); use
the reverse command.

When two machines write at the same time, the record that is last in the record order wins. The record order is the
time, then the machine id, then the sequence number. All machines use the same order, so they show the same result.
For example, a close at 10:00 on one machine and a reopen at 10:05 on another machine give an open change.

A closed change cannot go into the queue. `queue add` refuses it. `queue run` does not merge a pending entry of a
change that was closed after `queue add`. The entry becomes `failed` with the reason `#12 is closed`, and its hold is
released. To merge the change, reopen it and add it to the queue again.

Attestation kinds: `check`, `approval`, `origin`, `import`, `quarantine`, `revoke`, and `bisect`.
`bisect run` writes a `bisect` attestation of the first bad state; its payload is the result
(`layr attestations <state> --kind bisect`). Line and ref protection read a required check only as a
check attestation of the exact state and command with exit 0.
Each attestation is part of an immutable operation record with a machine signature.
It records the exact subject state, optional ChangeId, payload digest, signer, and time.
`--payload` reads a file as the caller and records its SHA-256 digest. It does not store the file contents.
The optional payload reference field is reserved for an object in an allowed layer.

`approve` writes an approval attestation. `approve --withdraw` appends a revoke attestation for the caller's approvals.
`attest revoke` revokes the caller's earlier attestations of the subject on this machine. Root revokes
those of every user of this machine (for example the service user of a check runner), never those of
another machine. A revoke that finds nothing to revoke prints a warning.
The original evidence remains in the log. A revoke cannot withdraw another caller's evidence.
`try` records a check attestation and its command result. Protected-line rules continue to use approvals and checks.
Layr before the attestation table wrote `approve`, `approve.withdraw` and `check` records. They stay in
the log with their signatures, but they are no evidence: they give no attestation.
Attestations do not keep a subject alive. Their records remain available while the subject lives.

State kinds `cache` and `derived` have optional `derived_from` and `context` fields.
They have no source parents, line, or ChangeId. Lines, source diffs, merges, and Git exports reject them.
A live ref can keep an auxiliary state and its source alive.
Existing cache snapshots keep their attachment to source states and their wire format for older clients.
They remain cache data outside source diffs. This change does not convert those snapshots into separate StateIds.
The new kinds support separate cache and derived states through the store API.

The database schema version stays at 6. New tables are additive. Optional fields are absent from old state records.

## Path rules of approvals

A line rule can ask for approvals by path. Both rules count approvals as `--approvals` does: by users or people other
than the authors of the states that the merge brings in.

- `--owner <path>=<principal>[,<principal>]` (repeatable): a change of the path (a folder, a file, or a glob) needs an
  approval of the merged state by one of these principals. Each owner group that the change touches needs its own
  approval, also when several rules match the line. The change is the difference between the merge base and the
  merged state. A person owner approves with `layr request approve --person`. An empty list removes the owners of
  the path.
- `--keep-approvals <path>[,<path>]`: an approval of an earlier version of the same change counts for a newer version
  when the newer version changed only these paths after it. A change of any other path needs a new approval. When
  several rules match the line, a path keeps approvals only when every rule keeps it. An empty value clears the list.

```sh
layr protect main --approvals 1 --keep-approvals docs,CHANGELOG.md
layr protect main --owner 'crates/store=person:ann@example.com,user:alice'
```

`merge` enforces both rules. `change show` and the model status count approvals of the exact state only.

## Ref protection and the merge queue

`protect` accepts ref patterns such as `env/prod` and `release/*`.
Ref rules accept `--update`, `--expect`, `--no-expect`, `--approvals`, `--approvers`, and `--check`.
They refuse `--direct`, `--no-direct`, `--rewrite`, `--path`, `--owner`, and `--keep-approvals`, which apply to lines.

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
`queue add` refuses a closed change, and `queue run` fails the entry of a change that was closed later
(see "Change numbers, titles and closing").
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
Without `fail_over`, probe errors never fail an entry.
This includes command exits, sandbox errors, missing outputs, invalid JSON, and refused links.
The queue stores each error and shows it in `queue list` and Behavior.
With `fail_over`, these errors fail the entry because the limit cannot be checked.
For screenshots, the limit applies to each screen's changed pixel share, including added or removed images.
Structure changes do not affect this limit. PNG file size does not affect it.
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
| `screenshots` | PNG pixels and optional HTML, XML, JSON, or text structure snapshots for each screen. |

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

## Screen probes

A screen probe writes files named `<screen>@<device>.<extension>` in its output folder.
For web routes, replace `/` with `_`, for example `_checkout@1280.png`.
Native names can use a device, for example `Checkout@iPhone-15.png`.
Each screen can have one PNG and one structure file.
A structure file can also exist without a PNG.
Files match by their relative path and screen name.

```toml
[probe.storefront-screens]
run = ["node", "screens.mjs"]
outputs = ["out/screens"]
kind = "screenshots"
tolerance = 8
fail_over = 0.05
```

Each red, green, blue, or alpha channel must differ by more than `tolerance` to change a pixel.
The default tolerance is 8 on the 0 to 255 scale.
JSON output includes every screen, its status (`added`, `removed`, or `compared`), dimensions, changed share, percent, and regions.
`changed_share` is a ratio. `changed_percent` rounds that ratio to one decimal percent.
The denominator includes every pixel present in either image. New or absent pixels count as changed.
Added and removed images have a changed share of 1.
Images with different dimensions report `size_changed` and both dimensions.
Up to 16 boxes cover the changed pixels. Nearby boxes merge when their gap is less than 8 pixels.
If more than 16 boxes remain, the nearest boxes combine until at most 16 remain.
Box coordinates start at the top left. Width and height count pixels.
PNGs stay in the derived state beside the structure files. Existing result reuse and garbage collection rules apply.
The JSON paths and output state IDs locate these files.

A screenshot `fail_over = 0.05` fails `probe diff` or a queue entry if any screen changes by more than 5 %.
The limit uses the exact share before display rounding.
Structure-only screens have a null pixel share and do not trigger this limit.
A plain diff reads stored results. It executes no commands.

Each structure file uses the same name as its PNG, with one of these extensions:

| Extension | Normalization |
| --- | --- |
| `.html` | One tag or text node per line, sorted attributes, no blank text nodes. Remove `nonce`, attributes that match `^data-.*id$`, the probe's `ignore` attributes, and generated ids (`id`, `for`, `aria-labelledby`, `aria-describedby`, `aria-controls` with a `:`, 4 or more digits, or a hex run of 8). Dates and times in text (`2026-11-06`, `2026-11-06T03:16:19Z`, `03:16`, `03:16:19`) become `<time>`. |
| `.xml` | One tag or text node per line, sorted attributes, no blank text nodes. Remove coordinate attributes by default. |
| `.json` | Sorted object keys, one field per line, stable indentation. Remove coordinate fields by default. |
| `.txt` | Compare text lines without normalization. |

XML and JSON omit these coordinate names by default: `bounds`, `frame`, `x`, `y`, `width`, `height`, `left`, `top`, `right`, `bottom`.
Set `ignore_attributes` to replace this list. An empty list retains every attribute or field.
XML and JSON retain frames as metadata for node outlines.
Accepted frames include Android bounds (`[x1,y1][x2,y2]`), frame strings, and JSON objects with `x`, `y`, `width`, and `height`.
JSON also accepts nested `origin` and `size` objects.
The probe must supply coordinates in PNG pixels. Convert native point coordinates before export when necessary.

Each screen reports `elements_added`, `elements_removed`, `text_changes`, and a unified `structure_diff` in JSON.
A tag attribute edit keeps the element count. Changed text nodes count as text changes.
XML text attributes and JSON fields named `text`, `label`, `value`, `title`, `content-desc`, or `name` count as text.
Other JSON scalar fields count as attributes. JSON objects and arrays count as elements.
Text added or removed with a node also counts as a text change.
`structure_regions` contains the boxes of changed native nodes with `change` values `text`, `added`, `removed`, or `attribute`.
HTML has no frame metadata, so its `structure_regions` list is empty.
A frame change alone does not change the normalized structure text.

A web probe can use Playwright to save the PNG and the DOM with CSS inlined by the probe:

```javascript
await page.setViewportSize({ width: 1280, height: 900 });
await page.goto("http://localhost:3000/checkout");
await page.screenshot({ path: "out/screens/_checkout@1280.png" });
// Inline the page's styles before this export.
await fs.writeFile("out/screens/_checkout@1280.html", await page.content());
```

Web screens can be rendered again from HTML with inlined CSS.
Native view trees cannot be rendered again. Capture a PNG for each native device.
An XCUITest can save a screenshot and a text snapshot:

```swift
let app = XCUIApplication()
app.launch()
let folder = URL(fileURLWithPath: "out/screens", isDirectory: true)
try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
try app.screenshot().pngRepresentation.write(to: folder.appendingPathComponent("Checkout@iPhone-15.png"))
try app.debugDescription.write(to: folder.appendingPathComponent("Checkout@iPhone-15.txt"),
                              atomically: true, encoding: .utf8)
```

A JSON accessibility export can replace the text file when node boxes are needed.
The probe command must copy the XCUITest artifacts into its output folder.

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
Owner decision 2026-11-11: a line holds no stack; a view joins a line and a set of overlays ([overlays](overlays.md) section 1). The stack rules above are what is built.
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
A completed run writes a signed `bisect.run` record with its good, bad, and first bad states.
The record also includes the command, each step, its exit code, and reuse status.
Use `layr bisect list [--json]` to read completed runs.
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


## Review threads, reactions and reviewers

A comment belongs to a change and to one exact version of that change.
A comment can name a file and a line or a line range of that version.
A comment that replies to no other comment starts a thread.
A reply belongs to the thread of the comment that it answers, also when it answers another reply.
Each comment is a signed record with its author and time.

```sh
layr comment <change> -m 'Why four lines?' --file src/main.rs --line 40-43 --state <rev>
layr comment reply <comment> -m 'They keep the write atomic.'
layr comment resolve <comment>
layr comment unresolve <comment>
layr react <comment> +1
layr react <change> heart --remove
layr review request <change> person:ann@example.com alice machine:ci-runner
layr review cancel <change> alice
layr change show <change> --json
```

- Without `--state` or `--version`, `comment` uses the newest version of the change. `--state` takes any
  revision of a version of the change. `--version v2` names the second version.
- `--file` is the same option as `--path`. `--line 40-43` names lines 40 to 43. A range needs a file.
- The text can also be the argument after the change (`layr comment <change> '<text>'`), as before.
- `comment reply`, `resolve` and `unresolve` take a comment id or the first characters of one (4 at least).
  Resolve and unresolve act on the thread of that comment.
- A reaction name has 1 to 32 characters from `a-z`, `0-9`, `_`, `+` and `-`, for example `+1`, `heart` or `eyes`.
  A writer has at most one reaction of a name on a comment or a change. `--remove` takes it away.
- A reviewer is `person:<login>`, `user:<name or uid>`, a local user name, or `machine:<name or id>`
  of a machine that wrote records in the project.
- A command that changes nothing (a resolved thread resolved again, a reaction that is already there, a reviewer
  who is already requested) writes no record and says so.
- These commands need the `try` permission, as `comment` does. `layr undo` skips their records; use the reverse
  command.

`change show` lists the threads, the reactions to the change, and the requested reviewers. Its JSON adds these keys:

- `threads`: one object per thread, in the order of the first comments. Keys: `id` (the first comment),
  `version` (state id), `version_label` (`v1`, ...), `path`, `line`, `line_end`, `resolved`, `resolved_by`
  (actor), `resolved_time`, `comment` and `replies`. A missing value is `null`.
- `reactions`: reaction name to count for the change. Each comment in `threads` also has `reactions`.
- `reviewers`: one object per active request, sorted by reviewer. Keys: `reviewer`, `status`,
  `approved_version`, `commented`, `requested_by` (actor) and `requested_time`.

The `comments` key stays: it lists every comment in record order. A comment on a range also has `line_end`.

A reviewer has one of these statuses:

- `approved`: the reviewer has an active approval of the newest version. The approval completes the request.
  No other record is written.
- `outdated`: the reviewer approved an older version only. `approved_version` names it.
- `commented`: the reviewer wrote a comment on the change and approved no version.
- `pending`: neither.

An approval counts as `layr approve` and `layr request approve --person` write it. For `user:<uid>`, the
approval and the comments must come from a machine of the project perspective, as for protection rules.
For `machine:<id>`, any user of that machine counts. For `person:<login>`, only records with the pinned key of
that person count.

### Records and concurrent writes

Machines can write review records at the same time and sync later. Each machine gets the same result from the same
records, in any arrival order.

| Operation                                 | Data key         | Fields                                                    |
|-------------------------------------------|------------------|-----------------------------------------------------------|
| `comment`                                 | `comment`        | the comment; `line_end` next to it for a range            |
| `comment.resolve`, `comment.unresolve`    | `thread_status`  | `change_id`, `thread`, `resolved`, `prev`                 |
| `reaction.add`, `reaction.remove`         | `reaction`       | `change_id`, `target` (comment or change id), `name`, `on` |
| `review.request`, `review.cancel`         | `review_request` | `change_id`, `reviewer`, `active`, `prev`                 |

- A thread status and a reviewer request use `prev`: the ids of the records of the same thread, or the same
  change and reviewer, that the writer saw. The records that no other record names are the heads.
  When the heads agree, they give the value. When they disagree, two machines wrote at the same time:
  the thread stays open and the request stays active. The next record names all heads and decides.
  The record time does not decide, so a machine with a wrong clock cannot hide a later change.
- A reaction belongs to its writer: a person with a pinned key, else one user of one machine.
  The newest record of that writer for the target and the name decides. A machine writes in sequence,
  so its sequence number orders its records.
- A reply to a comment that has not arrived yet starts its own thread until that comment arrives.
- The range end is next to the comment and not in it, so layr versions without ranges still read the comment
  (as a comment on the first line).
- Person requests (`layr request comment --person`) make comments and replies. Resolve, reactions and reviewer
  requests are machine records only.

The shared model (`layr_model::Project::review` and `change_show`) returns the same keys as the CLI.

## Person signing keys

`layr person keys` creates an Ed25519 signing key with the person age identity.
Only the encrypted signing key is stored in `signing.age`.
`person show --json` includes the public `signing_key`.
The person identity opens the signing key in memory.

For a file or S3 store, publish the public record and encrypted signing key with `--store`:

```sh
LAYR_PERSON_LOGIN=ann@example.com layr person keys --paper-key-out /private/ann-paper --store file:///path/to/store
layr access person-key ann@example.com <public-signing-key>
layr access grant person:ann@example.com maintainer
layr protect main --approvers person:ann@example.com --approvals 1
```

The store objects are `people/<login>/person.json` and `people/<login>/signing.age`.
An owner pins the public signing key in a signed access record.
Person principals use `person:<login>` in access, deny, and protection rules.
They receive no permissions from the executor's operating-system account.

## Person requests

A person submits a signed action to an immutable object at `requests/<id>.json`.
The requester creates a UUIDv7 id.
The shared `layr-store::request` builder and signer also build for wasm32.
The v2 object contains `v: 2`, `id`, `person`, `project`, `epoch`, `ct`, and `sig`.
`epoch` is `root` for the current project vault recipient from `store.json`.
`ct` is standard base64 with padding of age-encrypted exact signed v1 envelope bytes.
`sig` is the lowercase hex Ed25519 signature of `b"layr/request/v2\0"` followed by
the decoded age ciphertext bytes, not the base64 text.
The gateway verifies this outer signature and binds the person, project and object path.
It does not decrypt the action.
Inside the encryption, the v1 envelope contains `r`, the exact JSON body, and `s`,
its Ed25519 signature. The body includes the project, person, creation time, expiry
and action. Action payloads and comment text are encrypted.
The default expiry is 24 hours after creation.
Times use epoch milliseconds.

```sh
layr request approve <state> --into main --person --remote origin
layr request withdraw <state> --into main --person
layr request queue-add <state> --into main --person
layr request queue-remove <entry> --person
layr request ref-move release/v1 <state> --expect <old-state> --person
layr request ref-move release/v1 <state> --expect none --person
layr request comment <change> 'Please explain this line.' --path src/main.rs --line 42 --person
layr request declassify <state> 'src/config.rs@<id-from-refusal>' --person
layr requests apply --remote origin --json
```

Set `LAYR_PERSON_KEY_FILE` to the private person opener before submission.
`--remote` defaults to `origin`.
Approval and queue requests use `main` unless `--into` names another line.
`--expires` sets an explicit expiry in epoch milliseconds.
A ref request always compares its expected value with the current ref.
`--expect none` requires the ref to be absent.

A trusted executor verifies the outer signature against the pinned person key,
decrypts with the project identity, binds the inner and outer id, project and person,
and verifies the inner signature against that same pinned key.
It refuses legacy plaintext objects with `plaintext requests are not accepted`.
It checks the current access and protection rules as that person.
It refuses expired requests, unknown keys, stale ref values, and unauthorized actions.
Each refusal is a signed `request.refused` record with a plain reason.
Unverified request bytes are not copied into refusal records.

The normal operation and the original person proof share one signed record.
The inner proof is a string in `data.request` to preserve its exact bytes.
`data.encrypted_request` retains the exact v2 object, and `data.request_digest`
hashes that object for replay detection. Both proofs are in the normal encrypted
record stream. The shared model verifies their signatures and routing bindings.
Person approval attestations retain that signature and identify `person:<login>` as the signer.
Person approvals count in merge and ref protection checks.
A withdrawal can revoke only that person's evidence.
Comments appear in `change show` and the shared model.

Browser callers use these wasm-compatible shared Rust APIs:

```rust,ignore
layr_store::request::seal(signed_v1: &[u8], epoch_recipient: &str, person_key: &SigningKey) -> Result<Vec<u8>>;
layr_store::request::open(v2: &[u8], identity: &str) -> Result<Vec<u8>>;
```

`seal` verifies the inner signature and emits a v2 object with epoch `root`.
`Sealed::parse(bytes)` and `Sealed::verify(&VerifyingKey)` verify the outer envelope
without decryption. The caller binds its public routing fields to the authenticated
login, project and path. `open` returns exact signed v1 bytes and checks inner/outer
routing equality; callers must verify the outer signature before opening and verify
the returned `Signed` against the pinned person key before applying an action.
`seal_for_epoch(signed_v1, epoch, epoch_recipient, person_key)` supports a caller-selected
project epoch for rotation; the current CLI submits and accepts epoch `root`.

The executor claims each id at `requests/claims/<id>.json` with a machine signature.
Each claim is create-only and binds the project, request, and executor machine.
Another trusted machine stops the pass at a valid claim. Later requests wait. `requests apply`
returns exit code 2, and the pass does not run the queue, while a claim is waiting.
For encrypted stores, an untrusted claimant causes a signed refusal: `claimed by an untrusted machine`.
Invalid claims and invalid claim signatures also cause signed refusals there. For Cloud API v1,
the gateway checks each claim at creation. A claim by another machine stops the pass, even when
its key is not in this clone. A malformed claim, or a bad signature from a locally known machine,
stops the pass with an error. A claim with an unknown key waits without a local signature check.
After a crash, the owning machine resumes from its atomic local result record.
Claims never expire or transfer automatically.
Keep the owning machine's local records until its results reach the store.
A replayed id is ignored, including after reindex.
An existing id with changed content fails closed.
