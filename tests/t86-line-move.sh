#!/bin/bash
# `layr line move`: a line's working folder at a chosen path outside the store (same file system),
# on every backend. Daily commands work from the moved folder and its subfolders.
. "$(dirname "$0")/lib.sh"

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null
sample $T/src; cp -R $T/src/. $U/
(cd $U && git add -A && git commit -qm "upstream 1" && git push -q origin main)
l git clone $R p -q >/dev/null
M=$LAYR_ROOT/projects/p/lines
cd $M/main || exit 1
l line new -q w >/dev/null
W=$T/outside/work
mkdir -p $T/outside

# Refusals change nothing.
mkdir $T/outside/exists
out=$(l line move w $T/outside/exists 2>&1); rc=$?
eq "an existing path is refused" 1 "$rc"; has "existing path message" "already exists" "$out"
out=$(l line move w $T/nodir/work 2>&1); rc=$?
eq "a missing parent is refused" 1 "$rc"; has "missing parent message" "does not exist" "$out"
out=$(l line move w $LAYR_ROOT/inside 2>&1); rc=$?
eq "a path inside the store is refused" 1 "$rc"; has "inside store message" "inside the store" "$out"
out=$(l line move w 2>&1); rc=$?
eq "a line in the store cannot move back" 1 "$rc"; has "already in store message" "already in the store" "$out"
dev() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_dev)' "$1"; }
for other in /dev/shm /tmp "$HOME"; do # a folder on another file system, when one exists
    if [ -d "$other" ] && [ "$(dev $other)" != "$(dev $T)" ]; then
        out=$(l line move w $other/layr-move-x 2>&1); rc=$?
        eq "another file system is refused" 1 "$rc"; has "other file system message" "another file system" "$out"
        no "nothing was created there" test -e $other/layr-move-x
        break
    fi
done
ok "the line is still in the store" test -d $M/w/src

# Move out.
ok "line move" l line move w $W
no "the store folder is gone" test -e $M/w
ok "the folder is at the new path" test -f $W/src/main.rs
eq "line list --path shows the new path" "$W" "$(l line list --json | python3 -c 'import json,sys; print([r for r in json.load(sys.stdin) if r["name"]=="w"][0]["path"])')"
ok "the store index has an entry" grep -q "$W" $LAYR_ROOT/line-paths.json

# Daily commands from the moved folder and a subfolder.
cd $W || exit 1
eq "status is clean" "" "$(l status --short)"
echo two >> notes.txt
echo new > added.txt
ok "add" l add added.txt
ok "save" l save -a -qm "in moved folder"
eq "status is clean after save" "" "$(l status --short)"
has "log shows the save" "in moved folder" "$(l log)"
echo three >> notes.txt
has "diff shows the change" "three" "$(l diff)"
l save -a -qm "second" >/dev/null
ok "reset --hard" l reset --hard head~1
no "reset removed the line" grep -q three notes.txt
cd $W/src || exit 1
echo more >> main.rs
has "status from a subfolder" "main.rs" "$(l status --short)"
ok "save from a subfolder" l save -a -qm "from subfolder"
has "du from a subfolder" "w" "$(l du)"
cd $W || exit 1
ok "git reads at the new path" git log --oneline
has "git head is the line head" "$(l rev-parse --git head)" "$(git rev-parse HEAD)"
ok "git push to a local bare remote" l git push origin head:refs/heads/feat
eq "the remote has the branch" "$(l rev-parse --git head)" "$(git --git-dir=$R rev-parse feat)"

# Merge into main from the moved folder.
cd $M/main || exit 1
ok "merge the moved line" l merge w --expect "$(cd $W && l rev-parse head)"
ok "main has the work" grep -q more src/main.rs

# du.
has "du lists the moved line" "w" "$(cd $M/main && l du)"
has "du --store lists it by line name" "\"p/lines/w\"" "$(l du --store --json)"

# Rename keeps the link.
ok "line rename" l line rename w v
ok "the moved folder stays" test -f $W/src/main.rs
eq "the path follows the rename" "$W" "$(l line list --json | python3 -c 'import json,sys; print([r for r in json.load(sys.stdin) if r["name"]=="v"][0]["path"])')"
cd $W || exit 1
has "the folder finds the renamed line" "v" "$(l line list --show-current)"

# Move back, then out again.
cd $M/main || exit 1
ok "move back" l line move v
ok "the folder is in the store again" test -f $M/v/src/main.rs
no "the old path is gone" test -e $W
cd $M/v || exit 1
eq "status in the store folder" "" "$(l status --short)"
cd $M/main || exit 1
ok "move out again" l line move v $W
ok "folder at the new path" test -f $W/src/main.rs

# Delete a moved line.
ok "line delete of a moved line" l line delete -D v
no "the moved folder is deleted" test -e $W
no "the index entry is gone" grep -q "$W" $LAYR_ROOT/line-paths.json
no "the line is gone" l rev-parse v

finish
