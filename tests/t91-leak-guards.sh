#!/bin/bash
# Failure paths leave no temporary data: a cache copy that fails (save --caches), a forced save,
# a trash entry that cannot be deleted, saved scans of every gc path, a stray cache snapshot
# (fsck), and a temporary clone of a dead dedup run.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p
cd $P/lines/main || exit 1
l config regenerable '["a-cache","z-cache"]'
l line new -q w
cd $P/lines/w || exit 1
work_copies() { ls "$P/meta/work" 2>/dev/null | grep -c "^$1" || true; }

mkdir -p a-cache z-cache; echo one > a-cache/one; echo two > z-cache/two
echo c1 >> notes.txt; l save -q
l save --caches -q
echo three > z-cache/secret; chmod 000 z-cache/secret
echo c2 >> notes.txt
no "a save with caches fails when a cache cannot be copied" sh -c 'cd "$1" && "$2" save --caches -q' _ "$PWD" "$LAYR"
eq "the failed save leaves no copy in the work folder" "0" "$(work_copies prepared-)"
chmod 644 z-cache/secret; rm -f z-cache/secret

# A stray snapshot: a cache snapshot folder that no state record names.
sid=$(ls "$P/states" | grep -v '@' | head -1)
mkdir "$P/states/$sid@stray"
has "fsck reports a cache snapshot without record" "cache snapshot without record" "$(l fsck 2>&1)"
l fsck --repair >/dev/null 2>&1
ok "fsck --repair removes it" test ! -e "$P/states/$sid@stray"

echo four >> notes.txt; l save --force -q
eq "a forced save leaves no prepared layer" "0" "$(work_copies prepared-save-)"

# A trash entry that cannot be deleted must not keep the emptier running.
# Root deletes what a read-only folder keeps, so root locks the file instead (Linux only).
mkdir -p "$LAYR_ROOT/trash/stuck"; : > "$LAYR_ROOT/trash/stuck/x"
root_linux=0
[ "$(id -u)" = 0 ] && [ "$(uname -s)" != Darwin ] && root_linux=1
if [ "$root_linux" = 1 ]; then chattr +i "$LAYR_ROOT/trash/stuck/x"; else chmod 500 "$LAYR_ROOT/trash"; fi
timeout 60 "$LAYR" __empty-trash "$LAYR_ROOT/trash" >/dev/null 2>&1
rc=$?
if [ "$root_linux" = 1 ]; then chattr -i "$LAYR_ROOT/trash/stuck/x"; else chmod 700 "$LAYR_ROOT/trash"; fi
eq "the emptier ends although one entry stays" "0" "$rc"
has "the stuck entry is listed in trash.errors" "trash/stuck" "$(cat "$LAYR_ROOT/trash.errors")"
printf '%s\n' "trash/x: stuck" > "$LAYR_ROOT/trash.errors"
warn=$(cd "$P/lines/main" && "$LAYR" gc -n 2>&1 >/dev/null)
has "gc prints the lines of trash.errors" "could not be deleted" "$warn"
rm -f "$LAYR_ROOT/trash.errors"

if [ "$(uname -s)" = Darwin ]; then
    # Saved scans of a removed tree: every gc path removes them, a dry run keeps them.
    mkdir -p "$P/meta/scans"; stale="$P/meta/scans/11111111-2222-3333-4444-555555555555-stale"; : > "$stale"
    (cd "$P/lines/main" && "$LAYR" gc -n -q)
    ok "a dry run keeps a stale saved scan" test -e "$stale"
    (cd "$P/lines/main" && "$LAYR" gc --all-projects -q)
    ok "gc --all-projects removes a stale saved scan" test ! -e "$stale"
fi

# A temporary clone of a dead dedup run (.layr-dedup-<pid>, no live process has that pid).
: > a-cache/.layr-dedup-2000000000
l gc --dedup -q
ok "gc --dedup removes the clone of a dead run" test ! -e a-cache/.layr-dedup-2000000000
finish
