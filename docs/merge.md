# The merge engine

This page is part of the [Layr documentation](../README.md#documentation).

## Merge engine

Change lists of both sides come from `btrfs send --no-data` against the common base. A path
changed on one side takes that side (a reflink copy). Tree rules cover moves (inode level),
folder deletes against new files, links, executable bits, file/folder type changes, add/add,
rename/rename and delete/modify. Files changed on both sides use the driver from
`.gitattributes` or `.layrattributes`:

| driver            | behavior                                                                          |
| ----------------- | --------------------------------------------------------------------------------- |
| `text` (default)  | three-way line merge (`git merge-file`)                                           |
| `union`, `ours`, `theirs`, `binary` | as in git                                                       |
| `blocks`          | binary merge by byte ranges from the change lists: allowed when both sides kept the inode and size and the ranges do not overlap; `merge-verify=<name>` runs a check command |
| `<name>`          | `merge.driver.<name>` command with `%O %A %B %P`, run as the caller of the merge     |

### Derived files

Declare outputs in `.layrattributes` or `.gitattributes`:

```text
package-lock.json derive=npm
```

Set commands in the project config, outside the versioned tree:

```sh
layr config derive.command.npm 'npm install --package-lock-only'
layr config derive.inputs.npm 'package.json packages/*/package.json'
```

- Derive runs when a merge makes a new tree, including pick and replay.
- A fast-forward keeps the exact target state, including its derived files.
- A changed input or output on either side starts the rule.
- Inputs use whitespace-separated path patterns with Git ignore syntax.
- Derived outputs bypass the file merge. Literal output paths may be absent before the merge.
- Rules run by name in a stable order. Dependencies run first when their outputs match another rule's inputs.
- Dependency cycles become conflicts. A failed command keeps the ours output and adds a conflict to the state.
- Commands run as the caller in a private copy with a clean environment, as command drivers do.
- Only declared regular output files return to the result. Commands cannot return symbolic links.
- Commands have the caller's network access. Layr does not isolate the network or manage registry caches.
- Pin tool and dependency versions. Use commands that produce the same output from the same inputs.
- Layr does not run commands twice to check determinism.

### Batch lines and hard reset

```sh
layr line new trial head --count 10 --json
layr --project app --line trial-1 reset --hard main
```

The `line new` command creates `trial-1` through `trial-10` after validation of all names.
Each line has one ordinary `line.create` record. Existing log readers need no format change.
If a later line fails, completed lines stay valid. The error names those lines.
The command removes incomplete line folders and temporary snapshots.

On btrfs, batch creation submits standard snapshots with at most eight workers.
The kernel can share transaction commits between concurrent requests.
The supported ioctl API does not guarantee one transaction for all snapshots.

Measured on 2026-10-22 UTC in OrbStack `layr-dev`, Linux `7.2.9-orbstack-00391-g2b298d05fbd4`.
The debug binary runs as root without the service on a fresh btrfs loop filesystem with 100 source files.
The table shows medians of three trials. The order alternates between batch and single commands.

| Lines | Separate `line new` commands | One `line new --count` command | Less time |
| ---: | ---: | ---: | ---: |
| 10 | 1.585 s | 1.192 s | 24.8% |
| 100 | 19.324 s | 6.572 s | 66.0% |

The result supports retaining eight concurrent snapshot workers.
These times include command startup and database writes. They do not isolate the transaction cost.
Individual trials vary. The result does not establish a constant time for any batch size.
Run `sudo env LAYR=$PWD/target/debug/layr bash benchmarks/branch-batch.sh` to repeat the test.
See the [raw trials](../benchmarks/results/cloud-branch-batch.jsonl).

Hard reset can replace an idle btrfs line with a snapshot of the target state.
The copy path preserves local files, caches, overlays, and active process references when a swap is unsafe.
A call from inside the line also uses the copy path to preserve the caller's current directory.
The snapshot path keeps the existing `.git` subvolume. An ordinary `.git` directory uses the copy path.

### Conflict radar

`layr status` lists path overlaps with other live lines of the same project.
`layr status --json` returns an `overlaps` array with `path`, `line`, `owner`, and `changed_at`.
`changed_at` uses Unix milliseconds from the other file, or its state for a deletion.

Radar compares paths against each pair's common base with the existing change lists.
Each readable overlay uses its own common base.
This line uses its current content. Other lines use their newest saved state, or their head when newer.
Unsaved changes in other lines appear after `layr save`.
Project read access and overlay reader rules apply. Unreadable private paths stay hidden.
Short and porcelain status output keeps its existing format.
Radar writes no permanent records. A later event API can expose these results.

