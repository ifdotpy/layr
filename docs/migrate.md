# Move a project to the object store (`layr migrate`)

This guide is for a person who has a layr store with projects and wants to move one project to
the new storage. You do not need to know the history of this work.

## What changes

- Today layr keeps each saved state as a full folder copy or a thin layer (a folder with only the
  changed files). These copies are the "old snapshots", in `states/` and `deltas/` of the project.
- The object store (`meta/objects` of the project) keeps each file content once, compressed, and
  each folder as a small "tree" object. A state then names one tree and one "times table" (the
  modification time of every file).
- `layr migrate` writes the tree of every saved state, checks each one against its old snapshot,
  and adds the tree to the state. Your lines, history, tags and parked work keep their names and
  ids.
- The old snapshots stay until you delete them with `--delete-old`.

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
   old snapshots: `layr migrate --delete-old` frees 1.2 GiB in 55 snapshot(s) (line heads keep theirs)
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
   old snapshots: deleted 55 snapshot(s), 1.2 GiB freed; their states come back from the object store on read
   ```

3. The snapshots of line heads stay: daily commands (`status`, `save`) read them.
4. When a command needs an old state as a folder (for example `layr diff <old> <older>`), layr
   makes it again from the object store. This takes a moment the first time.

After `--delete-old`, you cannot go back to the old storage on this machine.

## Go back before `--delete-old`

Before `--delete-old`, the old snapshots are still the full truth. To undo the migration:

1. Stop everything that writes to the project (see above).
2. Delete the object store and the migration table of the project:

   ```sh
   rm -rf ~/Projects/layr-store/projects/<project>/meta/objects
   rm -rf ~/Projects/layr-store/projects/<project>/meta/migrate
   ```

3. Check the project with `layr fsck`.

The history keeps the records that named the trees, but without the object store they change
nothing: every command reads the old snapshots again. A later `layr migrate` starts from the
beginning.

## New saves after the migration

A save still writes a snapshot in this version. Run `layr migrate` again from time to time: it
moves only the states made since the last run.

## Remotes

- This version moves only the store on your machine.
- Git remotes (`layr git push`) do not change.
- Layr remotes (`layr push <remote>`) keep the old format (store format v2). Pushes and fetches
  work as before.
- The new remote format (store format v3) comes later. The plan: you push the migrated project to a
  new, empty remote in the new format, then you remove the old remote. An old remote is not changed
  in place.
- A teammate who clones from your remote gets the old format and can migrate their own copy with
  this guide.

## How long it takes

Measured on a MacBook Pro (M5 Max, APFS) with other work on the machine at the same time:

| project | states | migration (import and check) | repack | old snapshots | object store |
| --- | --- | --- | --- | --- | --- |
| 10,000 files, 60 states | 60 | MEASURE | MEASURE | MEASURE | MEASURE |
| the layr repository, last 60 commits | 60 | MEASURE | MEASURE | MEASURE | MEASURE |

The time grows with the number of states and the number of files per state: each state is checked
out and compared once. A project with large build output in its history takes longer.
