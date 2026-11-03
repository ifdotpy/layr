# layr follow

`layr follow` keeps a line at a remote branch. The daemon runs the same pass on a timer.

## Commands

- `layr follow <remote>/<branch>`: store the target, run one pass now.
- `layr follow --run`: run one pass now (the target must exist).
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
   Any error keeps the head too.
5. Caches, only after a merge:
   - Candidate lines: lines with an `export` record to that remote in the last 14 days, newest
     export first. A line is a candidate through the state of its export.
   - For each candidate, in order: a protected line needs its merge rule (`merge_met_rule`), and the
     squash-safe content check (`caches_contained`, #133) must pass. Then `pull_caches` takes the
     folders that the candidate has and no earlier candidate took.
   - `prune_cache(folder)` for each folder overlay (no-op hook, language adapters come later).
   - `layr save --caches` for the line.
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

    cp packaging/launchd/ws.layr.daemon.plist ~/Library/LaunchAgents/
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ws.layr.daemon.plist

Logs: `~/Library/Logs/layr-daemon.log`. Stop: `launchctl bootout gui/$(id -u)/ws.layr.daemon`.

## Linux

The systemd unit (`packaging/systemd/layr.service`) runs `layr daemon` as root. The follow tick is
part of it. No unit change is needed.
