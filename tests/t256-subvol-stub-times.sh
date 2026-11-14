#!/bin/bash
# ci:
# btrfs only: snapshot placeholders of nested subvolumes have no persistent inode times.
[ "$(uname -s)" = Linux ] && [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ] || {
    echo "$(basename "$0") (btrfs only): 0 passed, 0 failed"; exit 0;
}
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init p --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/p/lines/main"
# A native project without Git history does not need a Git bridge, so create its nested
# directory explicitly. Its contents must stay outside the parent snapshot.
btrfs subvolume create .git >/dev/null || exit 1
echo "not part of the state" > .git/marker
eq "the line's git directory is a subvolume" 256 "$(stat -c %i .git)"
echo saved >> notes.txt
l save -a -m "save with nested git" -q || exit 1
state=$(l rev-parse head)
id=$(python3 -I -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT id FROM states WHERE display=?", (sys.argv[2],)).fetchone()[0])' \
    "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "$state")
snapshot=$LAYR_ROOT/projects/p/states/$id
eq "the snapshot has a placeholder inode" 2 "$(stat -c %i "$snapshot/.git")"
ok "the placeholder is on a subvolume boundary" python3 -I -c \
    'import os,sys; assert os.stat(sys.argv[1]).st_dev != os.stat(sys.argv[1] + "/.git").st_dev' "$snapshot"

# Dropping the inode cache requires privileges. Instead, assert classification deterministically:
# a checkout must contain canonical zero, not the placeholder's kernel-invented modification time.
for round in 1 2; do
    ok "migration with a placeholder, round $round" l migrate
    ok "checkout with a placeholder, round $round" l objects checkout "$state" "$T/out-$round"
    eq "placeholder time is canonical padding, round $round" 0 \
        "$(python3 -I -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$T/out-$round/.git")"
    ok "placeholder mode is preserved, round $round" python3 -I -c \
        'import os,stat,sys; assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == stat.S_IMODE(os.stat(sys.argv[2]).st_mode)' \
        "$snapshot/.git" "$T/out-$round/.git"
    ok "verification with a placeholder, round $round" l objects verify "$state" --full
done
out=$(l status --short) || exit 1
eq "status stays clean with a placeholder" "" "$out"
out=$(l diff) || exit 1
eq "diff stays empty with a placeholder" "" "$out"
ok "verified placeholders permit old snapshot removal" l migrate --delete-old
ok "the saved state remains readable" l show "$state:notes.txt"
finish
