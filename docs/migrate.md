# Move a project from snapshot storage v1 to object store v2 (`layr migrate`)

This guide is for a person who has a layr store with projects and wants to move one project to
the new storage. You do not need to know the history of this work.

## What changes

Project storage v1 uses snapshots. Project storage v2 uses objects.
`layr objects stat --json` reports `"format":2` after migration.
The root container format and remote protocol versions are separate from these project storage versions.

- Today layr keeps each saved state as a full folder copy or a thin layer (a folder with only the
  changed files). These copies are the "old snapshots", in `states/` and `deltas/` of the project.
- The object store (`meta/objects` of the project) keeps each file content once, compressed, and
  each folder as a small "tree" object. A state then names one tree and one "times table" (the
  modification time of every file).
- `layr migrate` writes the tree of every saved state, checks each one against its old snapshot,
  and adds the tree to the state. Your lines, history, tags and parked work keep their names and
  ids.
- The old snapshots stay until you delete them with `--delete-old`.

After `reindex`, an absent state can have no owner and no deletion flag.
Migration reports these states and keeps their records unchanged.
It does not restore their absent contents.
An owner in the database or the record replay still requires the source state.
An absent source with an owner stops migration.

If you accept that a specific state remains unavailable, use `--leave-missing <state>`.
Repeat the option for each state that you accept.
Migration still tries to retrieve the source before it uses the exception.
It preserves the state record and its owners, but it does not create substitute contents.
The output names each exception. `meta/migrate/unavailable.json` records the exceptions separately from the verified states.
Supply the same exceptions again with `--resume`.

Details for developers: `docs/objects.md`, section 13.

## Before you start

1. Check that your layr has the command:

   ```sh
   layr migrate --dry-run
   ```

   If layr says `'migrate' is not a layr command`, install a newer layr first.
2. You need the admin role of the project. The person who made the project has it.
3. Measure the old storage of the project. Run this in a line of the project:

   ```sh
   layr du --store
   ```

   Note the size of `states` and `deltas`.
4. Check the free disk space:

   ```sh
   df -h ~
   ```

5. Make sure that the free space is at least:
   - 30 % of the old storage (the new objects; we measured 2 % to 30 %, see "How long it takes"),
   - plus the size of your largest line folder (one checkout at a time is made for the check),
   - plus 25 GB of margin for your other work.

   The old snapshots stay until the end, so the disk needs room for both.
6. Make sure that you have a copy of the project somewhere else (a Git remote or a layr remote).
   The migration does not delete anything without `--delete-old`, but a copy is good practice.

## Stop everything that writes to the project

The migration refuses to start while another layr command, an agent or the daemon works on the
store. During the migration other layr commands of the project wait until it ends.

1. Stop the agents and the terminals that run layr commands in this project.
2. Stop loops that run `layr follow --run`, `layr sync` or `layr queue run`.
3. On macOS, stop the layr daemon (it runs follow passes, syncs and gc in the background):

   ```sh
   launchctl bootout gui/$(id -u)/ws.layr.daemon
   ```

4. On Linux, stop the timers or services that run layr commands for this project.

If something still runs, the migration stops with a message that names it, for example
`a layr command holds the project lock (pid 1234: layr save -m ...)`. Wait for it, then run the
command again.

## Run the migration

Run the commands in a line folder of the project, for example
`cd ~/Projects/layr-store/projects/<project>/lines/main`. You can also name the project:
`layr migrate <project>`.

1. Look at the plan. Nothing changes in this step:

   ```sh
   layr migrate --dry-run
   ```

   Read these lines of the output:
   - `N state(s) to migrate`: the number of saved states that will move.
   - `would refuse now: ...`: something still runs. Stop it first.
   - `would refuse: N state(s) are on no store remote known here`: some old states are not on this
     machine. Run `layr fetch <remote>` for the remote that has them, then look at the plan again.
   - `snapshots of source states here`: the size of the old snapshots.
2. Start the migration:

   ```sh
   layr migrate
   ```

3. Watch the output. First, each line folder with unsaved work is saved:

   ```text
   line side: unsaved work saved as automatic state 1a2b3c4d
   ```

   Then one line per state:

   ```text
   12/60 state 1a2b3c4d cold: 10000 files, import 0.40 s, checkout 0.30 s, compare 0.20 s
   ```

   "hot" means the files stay as fast copies for line heads; "cold" means compressed history.
4. Read the end of the output. A good run ends like this:

   ```text
   migrated 60 state(s) (...) in 45.0 s; repack 12.0 s (...)
   states: 60 migrated now, 0 recorded from an earlier run, 0 migrated before, ...
   object store: 120 MiB (...)
   old snapshots: `layr migrate --delete-old` frees 1.2 GiB in 55 snapshot(s) 
   ```

   The exit code is 0. Every state was checked out again and compared with its old snapshot:
   every file, its bytes, its permission bits, its links, its extended attributes and its
   modification time.

### If the migration stops with a difference

The output names the state and the first differences, for example
`content differs: src/main.rs`. The exit code is 1.

1. Do not run `--delete-old`. Nothing was deleted, and your project works as before.
2. Save the output to a file.
3. Report it to the layr maintainers with the output.
4. After a fix, continue with `layr migrate --resume`.

### If the migration is interrupted

A crash, a reboot, or Ctrl-C can stop the run. The states that were checked before stay checked:
they are in the table `meta/migrate/map` of the project.

1. Run the same command with `--resume`:

   ```sh
   layr migrate --resume
   ```

2. It continues with the next state. A run without `--resume` refuses and tells you to use it.

## Check the result

Run these commands in a line folder of the project.

1. Your lines are all there:

   ```sh
   layr line list
   ```

2. The history is complete:

   ```sh
   layr log --oneline | head
   ```

3. An old file reads from the new storage. Take a state id from `layr log` and a file that
   existed then:

   ```sh
   layr show <state>:README.md
   ```

4. A new line from an old state works:

   ```sh
   layr line new check-migrate <state>
   ls ../check-migrate
   layr line delete -D check-migrate
   ```

5. The object store is complete:

   ```sh
   layr objects stat
   layr objects stat --json
   ```

   `states` is the number of migrated states. In the JSON output, `loose_bytes` is 0 after the
   repack (every object is in a pack).
6. The project passes its own checks:

   ```sh
   layr fsck
   ```

7. Start the daemon again (macOS):

   ```sh
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ws.layr.daemon.plist
   ```

## Delete the old snapshots

Do this only after a good run and after the checks above.

1. Delete the old snapshots:

   ```sh
   layr migrate --delete-old
   ```

2. Read the output:

   ```text
   old snapshots: deleted 55 snapshot(s), 1.2 GiB freed; their states remain in the object store
   ```

3. Line heads can also lose their old snapshots. Their object trees remain available.
4. `status`, `diff`, and file reads use the object trees directly.
5. Operations that require native folders can create temporary copies. The command removes these copies after its readers finish.

## Disable new object writes

To disable object writes for future saves, run:

```sh
layr config objects.enabled false
```

Keep `meta/objects` and `meta/migrate`. Existing states can depend on these objects, even without an earlier `--delete-old` command.
This setting does not convert object states back to the old format.

## New saves after the migration

A successful migration enables `objects.enabled`. New source states include a tree and a times table. This includes
named and automatic saves. Objects are complete before the signed record becomes visible.
The command then removes its native source copies, after readers release their pins.
A retry marker preserves this cleanup after a crash. Cache and derived states retain their native format.
Owner decision 2026-11-11: a cache is an ordinary overlay, and its states will go into v2 objects like any overlay ([cache overlays](cache-overlays.md) section 2). Today they keep the native format.
You do not need to repeat the migration for new saves.

Before `--delete-old` removes any snapshot, it checks the object checkout of every candidate
against its snapshot. A missing or damaged object stops the deletion.

## Remotes

- Local migration preserves each configured remote.
- Git mirrors (`layr git push`) keep their existing contract.
- Layr Cloud V1 sends portable tree/times states and typed objects. See `docs/objects.md`, section 14.
- File and S3 store remotes retain their store-v2 transfer contract. Native-only consumers use disposable copies.
- This command does not rewrite a remote in place.

## How long it takes

These workloads have not been measured on the integrated candidate. No performance result is claimed.

| project | states | migration (import and check) | repack | old snapshots | object store |
| --- | --- | --- | --- | --- | --- |
| 10,000 files, 60 states | 60 | Unmeasured | Unmeasured | Unmeasured | Unmeasured |
| the layr repository, last 60 commits | 60 | Unmeasured | Unmeasured | Unmeasured | Unmeasured |

The time grows with the number of states and the number of files per state: each state is checked
out and compared once. A project with large build output in its history takes longer.
