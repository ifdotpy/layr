#!/bin/bash
# ci: macos overlayfs
# A save that a crash cut short (#452, #453): its loose objects and its state record are empty files.
# Lines that do not name them keep working, fsck reports each one, gc maintains the rest and removes
# nothing while a kept record cannot be read, the line whose head cannot be read is deleted with
# --discard, a save of the same content writes the objects again, and fsck --repair moves the damaged
# objects aside and writes the record again from the signed record.
. "$(dirname "$0")/lib.sh"

mkdir -p $T/src/d
echo one > $T/src/a.txt
echo two > $T/src/d/b.txt
l init p --objects --from $T/src -q >/dev/null
O=$LAYR_ROOT/projects/p/meta/objects
cd $LAYR_ROOT/projects/p/lines/main
l line new -q w >/dev/null
cd ../w
touch $T/before-save
sleep 1
echo three > d/c.txt
l add -A >/dev/null
ok "the save of line w succeeds" l save -m second

# The crash: the objects and the state record that the save wrote are empty.
objects=$(find $O/objects -type f -newer $T/before-save)
record=$(find $O/states -type f -newer $T/before-save)
for f in $objects $record; do chmod 644 $f; : > $f; done
empty() { find $O/objects $O/states -type f -size 0 | wc -l | tr -d ' '; }
damaged=$(empty)
eq "the save wrote objects and one record, now empty" 1 "$([ "$damaged" -ge 3 ] && echo 1)"

cd ../main
ok "status works in another line" l status
cd ../w
# The head of w reads from its native copy where one is kept (the line folder is there, off macOS);
# else from its objects, which status reports as damage.
state=$(basename $record)
if [ -e "$LAYR_ROOT/projects/p/states/$state" ]; then
    ok "status works in the damaged line" l status
else
    out=$(l status 2>&1); rc=$?
    eq "status fails in the line whose head cannot be read" 1 "$([ "$rc" -ne 0 ] && echo 1)"
    has "status says what cannot be read" "state $state has no tree" "$out"
fi

out=$(l fsck 2>&1); rc=$?
eq "fsck fails" 1 "$rc"
eq "fsck names every empty object and record" "$damaged" "$(printf '%s\n' "$out" | grep -c ' is empty (use --repair)\| has no tree (use --repair)')"

l gc > $T/gc.out 2> $T/gc.err; rc=$?
eq "gc succeeds" 0 "$rc"
has "gc warns about what it cannot read" "cannot be read" "$(cat $T/gc.err)"
has "gc removes nothing while a kept record cannot be read" "removed 0 state record(s), 0 object(s), 0 hot blob(s)" "$(cat $T/gc.out)"

# The head of w cannot be read from objects. Where its native copy is kept (the line folder is there,
# off macOS), the final save reads that copy and writes its objects in full; else --discard deletes the
# line without a final save when that save cannot read the head.
cd ../main
l line delete -D --discard w > $T/delete.out 2> $T/delete.err; rc=$?
eq "a line whose head cannot be read from objects is deleted with --discard" 0 "$rc"
eq "line w is gone" 0 "$(l line list | grep -c '^[* ] *w$')"

# The same content saved again writes the objects that it shares with the damaged state.
cd ../main
l line new -q x >/dev/null
cd ../x
echo three > d/c.txt
l add -A >/dev/null
ok "a save of the same content succeeds" l save -m again
eq "the save wrote shared objects again" 1 "$([ "$(empty)" -lt "$damaged" ] && echo 1)"
eq "the save of x reads back" three "$(l show head:d/c.txt)"

out=$(l fsck --repair 2>&1)
has "repair moves a damaged object aside" "is empty: moved to $O/damaged/object-" "$out"
has "repair writes the record again from the signed record" "written again from the signed record" "$out"
eq "no empty object or record is left" 0 "$(empty)"
eq "the record is canonical again" 1 "$(grep -c '^{"times":"[0-9a-f]\{64\}","tree":"[0-9a-f]\{64\}"}$' $O/states/$state)"
out=$(l fsck 2>&1)
eq "fsck finds no damaged file after the repair" 0 "$(printf '%s\n' "$out" | grep -c 'is empty\|has no tree')"
has "fsck names what a state still reaches and is missing, and how to write it again" "layr objects import head --full" "$out"

ok "gc succeeds after the repair" l gc
ok "status still works" l status

# A head with only damaged objects, as on macOS everywhere: its native copy goes too. On OverlayFS
# that copy is a mount that no supported command removes, so this part runs on btrfs and APFS.
if [ -z "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    l line new -q v >/dev/null
    cd ../v
    touch $T/before-v
    sleep 1
    echo four > d/e.txt
    l add -A >/dev/null
    l save -qm fourth
    for f in $(find $O/objects $O/states -type f -newer $T/before-v); do chmod 644 $f; : > $f; done
    v=$(basename "$(find $O/states -type f -newer $T/before-v)")
    if [ -e "$LAYR_ROOT/projects/p/states/$v" ]; then
        mkdir -p $T/native-trash
        btrfs property set -f -ts "$LAYR_ROOT/projects/p/states/$v" ro false 2>/dev/null
        mv "$LAYR_ROOT/projects/p/states/$v" $T/native-trash/v && "$LAYR" __empty-trash $T/native-trash >/dev/null
    fi
    out=$(l status 2>&1); rc=$?
    eq "status fails in a line whose head is only damaged objects" 1 "$([ "$rc" -ne 0 ] && echo 1)"
    has "status names the damage" "state $v has no tree" "$out"
    cd ../main
    ok "status works beside it" l status
    ok "the line is deleted with --discard" l line delete -D --discard v
    ok "status works after" l status
fi
finish
