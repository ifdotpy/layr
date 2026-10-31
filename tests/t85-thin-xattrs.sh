#!/bin/bash
# A thin automatic state carries a changed extended attribute or ACL of an entry that looks
# equal to the base (same type, mode, size and times): an empty file, a folder, and a file
# with the same data. Another file changes, so the save makes a state.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" != Darwin ]; then
    echo "$(basename "$0") (APFS only): 0 passed, 0 failed"
    exit 0
fi

sample $T/src
: > $T/src/empty
mkdir $T/src/dir
echo data > $T/src/full
echo acl > $T/src/acl
xattr -w user.v old $T/src/empty
xattr -w user.v old $T/src/full
l init g --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q w >/dev/null
cd $LAYR_ROOT/projects/g/lines/w || exit 1
xattr -w user.v old dir

id() { printf %s "$1" | awk '{print $NF}'; }
echo first >> notes.txt
l save >/dev/null
sleep 1
ref=$T/ref
for f in empty full acl; do touch -r $f $ref.$f; done
xattr -w user.v new empty
xattr -w user.v new dir
xattr -w user.v new full
/bin/chmod +a "everyone allow read" acl
for f in empty full acl; do touch -r $ref.$f $f; done
echo second >> notes.txt
l save >/dev/null
# The layer of the automatic state in the store.
delta=$LAYR_ROOT/projects/g/deltas/$(ls $LAYR_ROOT/projects/g/deltas | sort | tail -1)
ok "the state has a layer" test -d "$delta"
eq "an empty file keeps its new attribute" new "$(xattr -p user.v $delta/empty 2>&1)"
eq "a folder keeps its new attribute" new "$(xattr -p user.v $delta/dir 2>&1)"
eq "a file with equal data keeps its new attribute" new "$(xattr -p user.v $delta/full 2>&1)"
has "a file keeps its new ACL" "allow read" "$(/bin/ls -le $delta/acl 2>&1)"
finish
