# Wave 2 in Layr: design decisions (lead)

Status: decided. Base: `cloud/integration` with core merged. Workers implement this file and
ask the lead before changing a decision.

## Facts from the code

- Protected lines already exist (`src/access.rs` `Rule`, `rule_for`): `expect` (merge only
  with `--expect <state>`), named checks that the merge runs on the merge result before the line
  moves, approvals of the exact state from allowed approvers who are not its authors
  (`src/cmd/mergecmd.rs` `check_approvals`), direct-commit and rewrite rules, per-path roles.
- Core now stores approvals and check results as attestations, and has refs with CAS
  (`src/cmd/refs.rs`), `changes/<id>` and `attestations/<id>` revisions.

## 1. Protection rules for refs (RFC 12.5, 29.4)

- `layr protect <pattern>` accepts ref patterns (`env/prod`, `release/*`) with the existing
  flags where they make sense: `--update <role>`, `--approvals <n> --approvers <principals>`,
  `--check <names>`, `--expect`/`--no-expect`. Line-only flags (`--direct`, `--rewrite`,
  `--path`) are refused for refs.
- `layr ref set` and `ref delete` on a protected ref: the caller needs the update role; with
  `expect` (default true, as for lines) `--expect` is required; the target state needs the
  required approvals (same counting as lines: exact state, allowed approvers, not authors) and,
  for each required check name, a passing `check` attestation on exactly that state whose
  command equals the configured command of that check. `ref set` never runs checks itself: a ref
  points at an existing state that was already checked (for example by the merge queue).
- Refusals name what is missing. Tests cover each rule.

## 2. Merge queue (RFC 13 phase 2D)

New command group `layr queue` (new concept, RFC 12.6 rule 3):

- `layr queue add <rev> [--into <line>]` adds the exact state of `<rev>` (a line, a state or
  `changes/<id>`; stored as the state id and its change id). Default target: the line's
  upstream or `main`.
- `layr queue list [--into <line>] [--json]`, `layr queue remove <entry>`.
- `layr queue run [--into <line>]` processes entries in order, one at a time:
  1. read the target head H;
  2. run the existing protected merge of the entry's state into the target through an internal
     entry point that takes the expected target head H (compare-and-swap on the target inside the
     lock of the move) and passes the entry's state as the existing `--expect` (which pins the
     source state); checks run on the exact merge result, derive rules apply, approvals of the
     entry's exact state are required as today. The public `merge` syntax does not change;
  2a. a merge result with conflicts never moves the target, for protected and unprotected lines
     alike: the entry fails with the conflicting paths;
  3. success: the line moves; the entry is done; record the result;
  4. failure (conflict, failed check, missing approval, moved head): the entry gets status
     `failed` with the reason and the queue continues with the next entry; a moved head only
     retries the same entry once against the new head.
- Entries and their status are signed records (`queue.add`, `queue.remove`, `queue.result`),
  so every machine sees the same queue and `undo` works.
- A daemon setting `queue.run = <line>[,<line>]` runs the queue for those lines after each new
  entry and every tick; without it the queue runs only by command.
- Result reuse: if a passing `check` attestation already exists for exactly the merge result
  state with the same command, the check is not run again (RFC 29.2).
- Not in this step: parallel speculative builds of several entries.

## 3. `bisect run` (RFC 4.9, 12.6)

`layr bisect run [--good <rev>] [--bad <rev>] [--auto] [--ref-log <ref>] -- <cmd>`:
- Default bad: the current line content (saved first as an auto state); default good: the line
  head's newest ancestor with a passing check attestation, else an error asking for `--good`.
- Candidates: the first-parent history between good and bad; `--auto` adds the line's auto
  states in time order between them; `--ref-log env/prod` uses the positions of that ref instead.
- Each step runs `<cmd>` with the existing `try` mechanism on the candidate state and writes a
  `check` attestation; a candidate with a matching passing or failing check attestation for the
  same command is not run again.
- Output: the first bad candidate with its state, message or auto-state origin, and the
  `origin` attestation payload digest when present. `--json` gives the full step list.

## 4. Secret detector (RFC 4.7)

- Source of secret values: files in overlays that are not shared (private overlays) visible to
  the committing user, plus files matched by the project config `secrets.paths` (patterns).
  Candidates: values of `KEY=VALUE` lines and whole file contents for files of at most 4 KiB,
  each of at least 12 characters, ignoring values that are only letters or only digits shorter
  than 20.
- Index: `HMAC-SHA256(k, value)` with a per-project random key kept in the root-only project
  folder; never the values. Rebuilt when a source file changes.
- Check points: `commit` and `merge` that produce a state of the shared set, `layr git push`
  and Git export, and `push` to any remote. Added lines of the diff are split into tokens
  (`[A-Za-z0-9_+/=.-]{12,}`) and compared by HMAC.
- A match refuses the operation with path, line and a declassify id `<path>@<first 16 hex of the
  HMAC>`, never the value, and prints the admin command
  `layr approve --declassify <path>@<id> <rev>` (`<rev>`: head of the target line). That writes
  an `approval` attestation with payload `{"declassify":{"path":"…","hmac":"<full hex>"}}`.
  A later operation is allowed when the same value appears at the same path and a non-revoked
  declassify attestation exists in the project; revoking it restores the refusal.
- Every overlay visible to the caller is a source.
- Limits stated in README: encoded or split secrets are not found; this adds to private
  overlays and does not replace them.

## Ownership

| Task | Worker | Files |
|---|---|---|
| 1 and 2 | core (continues) | access.rs, cmd/admin.rs protect, cmd/refs.rs, new cmd/queue.rs, mergecmd.rs entry points only |
| 3 and 4 | new worker `guard` | new cmd/bisect.rs, new src/secrets.rs, hooks in commit/merge/push paths (one call each) |

## 5. Views (RFC 3.0, 3.1)

Fact: a line already shows a stack, the shared head plus an ordered list of overlay layers
`{overlay, state}` (`src/overlay.rs` `Layer`, setting `stack.<line id>`), and the base key is a
hash of exactly that (`base_key`). A view is that stack made addressable.

- `ViewId` = full SHA-256 hex of the canonical JSON
  `{"format":1,"shared":"<state>","layers":[{"overlay":"<name>","state":"<state>"},…]}` in stack
  order. Access policy is not part of it (RFC 3.1). A line without overlays has the view
  `{"shared": head, "layers": []}`.
- `layr view [<rev or line>] [--json]` prints the ViewId and descriptor of a line's current
  stack (or of a state: no layers).
- `layr view save [<line>]` writes a signed record `view.save` with the descriptor; after that
  `views/<id>` resolves on every machine that has the record. Saving is idempotent.
- `revs::resolve("views/<id>")` returns the shared state, so every command that takes a state
  works. Commands that need the whole view use a new resolver `revs::resolve_view`:
  `layr branch <name> views/<id>` (new line with head = shared and the same stack, only if the
  caller may read every overlay layer; otherwise refused naming the overlay),
  `layr try views/<id> -- <cmd>` (runs in the assembled view, using the existing `assemble`).
- Attestations accept subject kind `view` (`layr attest <kind> views/<id>`). `try` on a line
  or revision whose view has layers writes its check with subject = the view; without layers the
  subject stays the state (unchanged behavior). `Db::check_exit` gets a view-subject variant.
- Readers: the descriptor names overlays and state ids, never content; reading the layers still
  needs overlay reader rights.

Owner: worker `merge` (continues), branch `cloud/views`.
