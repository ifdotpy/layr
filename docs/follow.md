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
- A Layr store supports `<remote>/main`. The creator sets `queue.merge-machine` to the trusted
  merge machine UUID and pushes that setting. The pass verifies its signed queue completion.
- Git remotes continue to support their named branches.

## One pass (`Repo::follow_pass`)

1. Fetch the Git branch, or fetch the Layr store heads and verify its main from signed records.
2. Stop when the remote state is the line head or an ancestor of it (result: up to date). If a cache folder has
   no cache saved with the head (an earlier pass stopped before its save, or a save moved the head), build and
   save the line's own caches as in step 5, with no other line as a source (result: up to date; caches saved
   with the head). Unsaved work skips this save. A damaged folder waits for its prune. A local merge of a line
   saves the caches with no build; on a followed line it leaves a mark (`meta/line-locks/<line>.unbuilt`), so
   this step builds and saves them on the next pass and removes the mark. A build that does not finish (its time
   limit, a compile error) leaves the mark too: the next pass builds again, and cargo goes on from the units it made.
   The prune of the pass has a limit of 10 min (`BACKGROUND_PRUNE_LIMIT`), not the 120 s of a command: no one waits
   for it, and a loaded machine makes a prune of seconds take minutes.
   For a Layr store with no unsaved work, each pass also checks for a shared compatible cache of the verified head. It can take a cache
   published after the head moved. The local record of each taken cache prevents a later pass from taking it again.
   A cache saved with an automatic state is accepted only when its source tree matches the verified head.
3. Stop when the line has unsaved work: a changed file or a conflict (result: `skipped: unsaved work`).
   Nothing is saved for it. An untracked file is not unsaved work: the merge keeps it out of the state,
   and refuses a remote state that writes its path (the pair is then recorded as in step 4).
4. Merge the remote state with `--expect <fetched state>`. A fast-forward when possible. The merge
   runs with `--refuse-conflicts`: a conflict discards the merge result, and the line keeps its head.
   Any error keeps the head too. A failed merge records the pair (fetched remote state, line head).
   A tick then skips the pair without output and without a new record, until the remote state or
   the line head changes. `layr follow` and `--run` always try.
5. Caches after a merge:
   - A Layr store fast-forward takes a shared compatible cache of the verified main, when one is available.
   - Candidate lines: lines with an `export` record to that remote in the last 14 days, newest
     export first. A line is a candidate through the state of its export. A remote with the same
     repository URL counts as that remote (`git@host:path`, `ssh://` and `https://` forms of one
     repository match), so a push over https is a source of a follow over ssh.
   - For each candidate, in order: a protected line needs its merge rule (`merge_met_rule`), and the
     squash-safe content check (`caches_contained`, #133) must pass. Then `pull_caches` takes the
     folders that the candidate has and no earlier candidate took. An error of one candidate (its
     cache names a state that is not on this machine, say) refuses only that candidate. When no
     candidate gave a cache, the result names the last refusal (`no cache taken: ...`).
   - The tracked files get the source times that the taken caches recorded (`.layr-times`,
     docs/cache-overlays.md section 20).
   - The build: the adapter of each folder overlay builds the project's standard commands into the
     folder (`warm`, as the line owner, limit 120 min; cargo: the `sweep` list with the project env; pnpm:
     `pnpm install --frozen-lockfile`, online, from a copy that a failure puts back). A
     change that came without a cache (a branch that no line here pushed) is built here. If the
     head moves during the build, the pass retries, at most three times. A failed build or a head
     that keeps moving leaves the line marked for the next pass. It does not save the cache.
   - `layr save --glasses` for the line: it prunes each folder first (the adapter's `prune`). The
     line lock keeps the checked head unchanged until the cache is saved and the mark is cleared. A separate
     cache build lock prevents a cache save from overlapping the build. An ordinary head save
     can proceed during the build; follow then builds the new head again. Cache operations take
     the cache build lock before the line lock.
6. Record the result in the project meta folder (`follow.json`, this machine only): time, ok, text.

A failed cache step does not undo the merge. Its text is part of the result.

Owner decision 2026-11-11: a cache is an ordinary overlay (glass) with no cache flags, so the
pass will save and take the cache glass as part of the line, not through `save --caches` and
`pull_caches` ([cache overlays](cache-overlays.md) section 2). The steps above are what is built.

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
A daemon of another build than the client (an install without a daemon restart) prints
`the layr daemon runs another layr build than this one; restart it`.

## Linux

The systemd unit (`packaging/systemd/layr.service`) runs `layr daemon` as root. The follow tick is
part of it. No unit change is needed.
