#!/bin/bash
# ci: macos overlayfs
# Undo loads every state it needs before it changes a file: a state that a store remote holds
# comes back from it, and a state that cannot load stops the undo with the files unchanged.
# A failed `line move` leaves the line at its old path with its unsaved work, and the store index
# is a copy of the project files (a missing index entry is found from the project file).
. "$(dirname "$0")/lib.sh"
# The store sits in a folder of the test file system, so that a folder beside it is on the same
# file system (on btrfs the store root is the mount point itself, see t86-line-move).
FS=$LAYR_ROOT
export LAYR_ROOT=$FS/store; mkdir -p $LAYR_ROOT

sample "$T/source"
# One project per check: a broken state in one check must not hide the next check's result.
new_project() { # new_project <name>: a project from the sample, with a file:// store remote
    l init "$1" --from "$T/source" "${@:2}" -q >/dev/null
    P=$LAYR_ROOT/projects/$1
    cd "$P/lines/main"
    l remote add archive "file://$T/archive-$1" --paper-key-out "$T/paper-$1" >/dev/null
}

# A state folder is named by the UUID that ids::display maps to the id: bytes 10..16, 6..10 and 0..6.
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
on_disk() { [ -e "$P/states/$(dashed "$1")" ] || [ -e "$P/deltas/$(dashed "$1")" ]; }
# The local tree of a state goes, as gc leaves it after eviction: the record and the store copy stay.
drop_tree() {
    local d
    for d in "$P/states/$(dashed "$1")" "$P/deltas/$(dashed "$1")"; do
        [ -e "$d" ] || continue
        if [ "$(uname -s)" = Darwin ]; then
            chflags -R nouchg "$d" 2>/dev/null || true
            /bin/chmod -R -N "$d" 2>/dev/null || true
            rm -rf "$d"
        elif [ "$(stat -f -c %T "$d" 2>/dev/null)" = btrfs ]; then
            btrfs property set "$d" ro false
            btrfs subvolume delete "$d" >/dev/null
        else
            # OverlayFS: a state is a mount (the CI job runs as root).
            if mountpoint -q "$d" 2>/dev/null; then umount "$d" 2>/dev/null || umount -l "$d"; fi
            rm -rf "$d"
        fi
    done
    # A state that is still on disk: gc evicts the copy the store holds, if no owner keeps it.
    if on_disk "$1"; then l config gc.evict on >/dev/null; l gc --repair -q >/dev/null; fi
}
tree_sum() { (cd "$1" && find . -path ./.git -prune -o -type f -print | sort | xargs cksum); }

# (a) Undo of an amend. The before state is a sibling of the head, so no later state keeps it and
# the store remote is its only other copy. Undo must fetch it back, not report it as deleted.
new_project amended --no-objects
echo one >> notes.txt; l save -a -m one -q
echo two >> notes.txt; l save -a -m two -q
two=$(l rev-parse head)
ok "push the history to the store remote" l push archive -q
echo amended >> notes.txt; l save -a --amend -m "two amended" -q
amend_op=$(l op log -n 1 | awk '{print $1}')
drop_tree "$two"
no "the before state is gone from this machine" on_disk "$two"
ok "undo of the amend fetches the before state from the store remote" l undo "$amend_op"
eq "the head is the state before the amend" "$two" "$(l rev-parse head)"
ok "the fetched state is on disk again" on_disk "$two"
has "the amended change is now uncommitted" "notes.txt" "$(l status --short)"

# (b) Undo of a restore whose states are freed (past the undo ring). The undo must stop before it
# changes a file: the old code fell back to a full restore, which dropped the later edit.
new_project restored
echo one >> notes.txt; l save -a -m one -q
one=$(l rev-parse head)
echo two >> notes.txt; l save -a -m two -q
l restore --source "$one" -- notes.txt >/dev/null
restore_op=$(l op log -n 1 | awk '{print $1}')
echo later >> notes.txt
# The undo ring holds no operation any more: the states of the restore are freed.
l config undo.ops 0 >/dev/null; l gc --repair -q >/dev/null
tree_sum "$P/lines/main" > "$T/sum.before"
latest=$(l op log -n 1)
out=$(l undo "$restore_op" 2>&1); rc=$?
eq "a stopped undo records no automatic save" "$latest" "$(l op log -n 1)"
eq "undo stops with exit 1 when the after state cannot load" 1 "$rc"
has "the error says the undo stopped" "cannot undo" "$out"
tree_sum "$P/lines/main" > "$T/sum.after"
eq "no file changed by the stopped undo" "$(cat "$T/sum.before")" "$(cat "$T/sum.after")"
eq "the later edit is kept" "later" "$(tail -1 notes.txt)"

# (c) A line move whose index write fails. The index path is a folder, so the write fails after the
# project file has the new path; the move must roll back both and leave the line at its old path.
new_project moved
l line new -q w >/dev/null
W=$FS/outside/work
mkdir -p "$FS/outside"
echo unsaved >> "$P/lines/w/notes.txt"
mkdir "$LAYR_ROOT/line-paths.json"
no "the move fails when the index cannot be written" l line move w "$W"
# The refusal is the index write, not the path: a path refusal would pass the check above too.
out=$(l line move w "$W" 2>&1)
no "the failure is not a path refusal" grep -q "another file system\|inside the store\|already exists\|does not exist" <<<"$out"
rmdir "$LAYR_ROOT/line-paths.json"
no "the failed move leaves no folder at the new path" test -e "$W"
ok "the working folder is at the old path" test -f "$P/lines/w/notes.txt"
has "the unsaved change is still in the old folder" "unsaved" "$(cat "$P/lines/w/notes.txt")"
has "a command in the old folder finds the line and its change" "notes.txt" "$(cd "$P/lines/w" && l status --short)"
no "the project file does not record the failed path" grep -q "$W" "$P/meta/line-paths.json"

# (d) The index is a copy of the project file. Without the index entry, a command in the moved
# folder still finds its line.
new_project indexed
l line new -q w >/dev/null
W=$FS/outside/indexed-work
echo unsaved >> "$P/lines/w/notes.txt"
ok "line move" l line move w "$W"
ok "the folder is at the new path" test -f "$W/notes.txt"
rm -f "$LAYR_ROOT/line-paths.json"
has "a command in the moved folder finds the line without the index" "notes.txt" "$(cd "$W" && l status --short)"
cd "$P/lines/main"
ok "line move back" l line move w
no "the moved folder is gone" test -e "$W"

# (e) A non-in-place undo keeps the saves and folder replacement under one lock.
# Write while it waits for that lock: the first capture must be discarded and
# undoing the undo must recover the late edit from the replacement capture.
new_project undo_gap
l line new -q victim
new_op=$(l op log --all -n 1 | awk '{print $1}')
echo early > "$P/lines/victim/early.txt"
barrier=$T/undo-barrier
mkdir "$barrier"
LAYR_TEST_SAVE_UNDO_BARRIER="$barrier" LAYR_TRACE=1 l undo "$new_op" >"$T/undo-gap.out" 2>&1 &
pid=$!
for _ in $(seq 1 500); do
    [ -f "$barrier/ready" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.02
done
if [ ! -f "$barrier/ready" ]; then
    touch "$barrier/release"; wait "$pid"; cat "$T/undo-gap.out"
    echo 'FATAL: undo did not reach its publication barrier'; exit 1
fi
eq "undo keeps its before-undo save unpublished at the barrier" "$new_op" "$(l op log --all -n 1 | awk '{print $1}')"
python3 - "$P/meta/lock" "$barrier" "$P/lines/victim/late.txt" <<'PYUNDO'
import fcntl
from pathlib import Path
import sys
import time
with open(sys.argv[1], 'a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    Path(sys.argv[2], 'release').touch()
    time.sleep(0.2)
    Path(sys.argv[3]).write_text('written during lock wait\n')
PYUNDO
wait "$pid"
eq "undo retries a capture changed during the project lock wait" 0 "$?"
ok "undo removes the created folder" test ! -e "$P/lines/victim"
undo_op=$(l op log --all -n 1 | awk '{print $1}')
ok "undo the undo restores the created line" l undo "$undo_op"
eq "undo of undo recovers the early file" early "$(cat "$P/lines/victim/early.txt")"
eq "undo of undo recovers the lock-wait edit" 'written during lock wait' "$(cat "$P/lines/victim/late.txt")"
# A head-check refusal must not publish a before-undo automatic save.
cd "$P/lines/victim"
echo changed >> notes.txt
l save -qam changed
cd "$P/lines/main"
latest=$(l op log --all -n 1)
no "undo refuses a created line whose head changed" l undo "$new_op"
eq "the refused undo adds no before-undo save" "$latest" "$(l op log --all -n 1)"

finish
