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

# Named states: `save -a -m` and `add` then `save -m` keep a changed attribute and ACL of a
# file whose data and times did not change.
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q n >/dev/null
cd $LAYR_ROOT/projects/g/lines/n || exit 1
l save -m base >/dev/null
for f in full empty; do touch -r $f $ref.$f; done
xattr -w user.v named full
/bin/chmod +a "everyone allow read" acl
for f in full; do touch -r $ref.$f $f; done
l save -a -m named >/dev/null
named=$(l rev-parse head)
delta=$LAYR_ROOT/projects/g/deltas/$(ls $LAYR_ROOT/projects/g/deltas | sort | tail -1)
has "save -a -m keeps a new ACL" "allow read" "$(/bin/ls -le $delta/acl 2>&1)"
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q vn "$named" >/dev/null
cd $LAYR_ROOT/projects/g/lines/vn || exit 1
eq "save -a -m keeps a new attribute" named "$(xattr -p user.v full 2>&1)"
cd $LAYR_ROOT/projects/g/lines/n || exit 1
touch -r empty $ref.e2
xattr -w user.v added empty
touch -r $ref.e2 empty
l add empty >/dev/null
l save -m added >/dev/null
added=$(l rev-parse head)
cd $LAYR_ROOT/projects/g/lines/main || exit 1
l line new -q va "$added" >/dev/null
cd $LAYR_ROOT/projects/g/lines/va || exit 1
eq "add then save -m keeps a new attribute" added "$(xattr -p user.v empty 2>&1)"
finish
