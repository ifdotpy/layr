# layr follow

`layr follow` keeps a line at a remote branch. The daemon runs the same pass on a timer.

## Commands

- `layr follow <remote>/<branch>`: store the target, run one pass now.
- `layr follow --run`: run one pass now (the target must exist).
- `layr follow --tick`: run the pass the daemon runs (a refused remote state is not retried).
- `layr follow`: print the target and the result of the last pass.
- `layr follow --remove`: remove the target. The line keeps its files.

## Target and rights

- Stored as project config `follow.<line id>` = `"<remote>/<branch>"`. The key is built in one
  place only: `follow::config_key`.
- Setting or removing a target needs the same right as `layr merge --expect` on the line.

## One pass (`Repo::follow_pass`)

1. Fetch: `layr git fetch <remote> <branch>`.
2. Stop when the remote state is the line head or an ancestor of it (result: up to date).
3. Stop when the line has unsaved work (result: `skipped: unsaved work`). Nothing is saved for it.
4. Merge the remote state with `--expect <fetched state>`. A fast-forward when possible. The merge
   runs with `--refuse-conflicts`: a conflict discards the merge result, and the line keeps its head.
   Any error keeps the head too. A failed merge records the pair (fetched remote state, line head).
   A tick then skips the pair without output and without a new record, until the remote state or
   the line head changes. `layr follow` and `--run` always try.
5. Caches, only after a merge:
   - Candidate lines: lines with an `export` record to that remote in the last 14 days, newest
     export first. A line is a candidate through the state of its export.
   - For each candidate, in order: a protected line needs its merge rule (`merge_met_rule`), and the
     squash-safe content check (`caches_contained`, #133) must pass. Then `pull_caches` takes the
     folders that the candidate has and no earlier candidate took.
   - The tracked files get the source times that the taken caches recorded (`.layr-times`,
     docs/cache-overlays.md section 20).
   - The build: the adapter of each folder overlay builds the project's standard commands into the
     folder (`warm`, as the line owner, limit 30 min; cargo: the `sweep` list with the project env). A
     change that came without a cache (a branch that no line here pushed) is built once here, so the
     lines from this line start warm. A failed or stopped build is one log line; the save still runs.
   - `layr save --caches` for the line: it prunes each folder first (the adapter's `prune`).
6. Record the result in the project meta folder (`follow.json`, this machine only): time, ok, text.

A failed cache step does not undo the merge. Its text is part of the result.

## Daemon

- Linux, root: unchanged, plus the follow tick. Default tick 60 s.
- macOS, LaunchAgent as the user (`layr daemon`): no root, no user switch. It serves only its own
  uid on its socket (default `~/Library/Caches/layr/layr.sock`). Its tick (default 120 s,
  `--tick <seconds>`) runs one follow pass for each followed line of each project, one at a time.
  It runs no gc and frees no state.
- macOS CLI: runs commands directly (no forwarding), as before.
- An error in one line is printed and does not stop the others.

## Install on macOS

    # The store your layr commands use. Empty means the default store:
    export LAYR_ROOT="${LAYR_ROOT:-$HOME/Library/Application Support/layr}"
    cp packaging/launchd/ws.layr.daemon.plist ~/Library/LaunchAgents/
    plutil -replace EnvironmentVariables.LAYR_ROOT -string "$LAYR_ROOT" ~/Library/LaunchAgents/ws.layr.daemon.plist
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ws.layr.daemon.plist

`LAYR_ROOT` must be the store your commands use. launchd gives the daemon no shell environment; without
it the daemon opens `~/Library/Application Support/layr`, and `layr follow` passes do nothing for your
store. After an earlier install, stop the agent first (below) and bootstrap again.
The daemon log's first line names the store (`... (store <root>)`).

Logs: `~/Library/Logs/layr-daemon.log`. Stop: `launchctl bootout gui/$(id -u)/ws.layr.daemon`.

`layr follow` prints one warning line when no daemon of this user serves the store: none runs, or
a daemon serves another store (follow passes then run only on `layr follow --run`). A daemon
built before the store root was reported prints `the layr daemon runs an older build; restart it`.

## Linux

The systemd unit (`packaging/systemd/layr.service`) runs `layr daemon` as root. The follow tick is
part of it. No unit change is needed.
