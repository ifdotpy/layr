#!/bin/bash
# ci: macos overlayfs
# Move a project to another volume (`layr project move`, #173 part B). A project with three lines, saves,
# a cache overlay and a parked change moves to a second file system and keeps working there. Refusals change
# nothing. A crash after the copy rolls back on the next command; a rerun completes the move. A crash after
# the switch finishes on the next command. Two more file systems: APFS images on macOS (attached as the user),
# loop file systems on Linux (CI runs as root).
. "$(dirname "$0")/lib.sh"
# The move's steps print their start and end (LAYR_TRACE): a hang shows where it stops.
export LAYR_TRACE=1

if [ "$(uname -s)" = Darwin ]; then
    # Images and mount points live outside the test root: hdiutil cannot mount an image at a
    # folder inside another mounted image ("Permission denied").
    VOLS=$(mktemp -d "${TMPDIR:-/tmp}/layr-move.XXXXXX")
    [ -n "$VOLS" ] && [ -d "$VOLS" ] || { echo "FATAL: cannot create the volume folder" >&2; exit 1; }
else
    VOLS=$T
fi
trap 'detach_all; cleanup; [ "$(uname -s)" = Darwin ] && rm -rf "${VOLS:?}"' EXIT

# new_volume <name>: a file system of its own, mounted at $VOLS/<name>. False when the host cannot.
new_volume() {
    local name=$1 mp=$VOLS/$1
    if [ "$(uname -s)" = Darwin ]; then
        hdiutil create -size 2g -fs APFS -type SPARSE -volname "layr-$name-$$" "$mp.sparseimage" >/dev/null 2>&1 || return 1
        mkdir -p "$mp" && hdiutil attach -nobrowse -mountpoint "$mp" "$mp.sparseimage" >/dev/null 2>&1 || return 1
        return 0
    fi
    if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
        # OverlayFS: a second file system of the same type as the test root, mounted by this test
        # (a folder of the test root would be the same file system, which a move refuses).
        local type
        type=$(findmnt -n -o FSTYPE -T "$T")
        truncate -s 2G "$mp.img" || return 1
        if [ "$type" = xfs ]; then mkfs.xfs -q -f "$mp.img"; else mkfs.ext4 -q -F "$mp.img"; fi || return 1
        mkdir -p "$mp" && mount -o loop "$mp.img" "$mp" || return 1
        mount --make-private "$mp" || return 1
        return 0
    fi
    new_fs "$mp" 2G
}

# unmount <name> and remount <name>. On the OverlayFS build a volume that is not mounted is a folder
# with no identity file, which is the same refusal.
unmount() {
    if [ "$(uname -s)" = Darwin ]; then hdiutil detach "$VOLS/$1" >/dev/null 2>&1 || hdiutil detach -force "$VOLS/$1" >/dev/null 2>&1
    elif [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then mv "$VOLS/$1/layr-volume.json" "$VOLS/$1/layr-volume.away"
    else umount "$VOLS/$1"; fi
}
remount() {
    if [ "$(uname -s)" = Darwin ]; then hdiutil attach -nobrowse -mountpoint "$VOLS/$1" "$VOLS/$1.sparseimage" >/dev/null 2>&1
    elif [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then mv "$VOLS/$1/layr-volume.away" "$VOLS/$1/layr-volume.json"
    else mount -o loop,user_subvol_rm_allowed "$VOLS/$1.img" "$VOLS/$1"; fi
}
detach_all() {
    [ "$(uname -s)" = Darwin ] || return 0
    local name
    for name in fast slow; do
        hdiutil detach "$VOLS/$name" >/dev/null 2>&1 || hdiutil detach -force "$VOLS/$name" >/dev/null 2>&1 || true
    done
}

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# The volume that the project record names (empty when the project has no record).
record_of() { python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('projects',{}).get(sys.argv[2],''))" "$LAYR_ROOT/project-volumes.json" "$1" 2>/dev/null; }

new_volume fast || { echo "SKIP project move: this host cannot make a second file system"; finish; exit 0; }
new_volume slow || { echo "SKIP project move: this host cannot make a third file system"; finish; exit 0; }
FAST=$VOLS/fast SLOW=$VOLS/slow
FASTP=$(cd "$VOLS" && pwd -P)/fast # volume add stores the resolved path

# A git bridge upstream with a small project: the projects are cloned from it.
R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null
printf 'one\n' > $U/notes.txt
printf 'target/\n' > $U/.gitignore
printf 'target regenerable\n' > $U/.layrattributes
(cd $U && git add -A && git commit -qm "upstream 1" && git push -q origin main)

# clone_project <name>: a project on the default volume (main), its main line unprotected in the macOS test mode.
clone_project() {
    l git clone $R "$1" -q >/dev/null || return 1
    if [ "${LAYR_TEST_DIRECT_MAIN:-}" = 1 ]; then
        (cd "$LAYR_ROOT/projects/$1/lines/main" && "$LAYR" protect main --remove >/dev/null 2>&1) || return 1
    fi
}

clone_project p || { echo "FATAL: cannot clone the project" >&2; exit 1; }
P=$LAYR_ROOT/projects/p
cd $P/lines/main || exit 1
for i in $(seq 1 8); do echo "save $i" >> notes.txt; l save -a -qm "save $i" >/dev/null; done
l line new -q a >/dev/null
l line new -q b >/dev/null
l line new -q c >/dev/null
cd $P/lines/a || exit 1
echo "in a" >> notes.txt; l save -a -qm "a work" >/dev/null
cd $P/lines/b || exit 1
echo "wip" > wip.txt; l park -q >/dev/null 2>&1
cd $P/lines/main || exit 1
mkdir -p target && head -c 20971520 /dev/urandom > target/big.bin
l save --overlays -q >/dev/null
ok "the 20 MB cache is an overlay" sh -c "$LAYR overlay list | grep -q target"
ok "the lines have git objects" test -f $P/lines/main/.git/objects/info/alternates

cd $T || exit 1
l volume add fast "$FAST" >/dev/null

# Refusals change nothing.
no "an unknown project is refused" l project move nope fast
no "an unknown volume is refused" l project move p nope
no "an overlay is refused" l project move "p%target" fast
ok "the refusals leave the project on main" test -d $P/lines/main
eq "the refusals leave no record" "" "$(record_of p)"
mkdir -p $FAST/projects/p
out=$(set -o pipefail; cd $T && l project move p fast 2>&1 | tee /dev/stderr); rc=$?
eq "a target with a folder of the same name is refused" 1 "$([ "$rc" -ne 0 ] && echo 1 || echo 0)"
has "the refusal says it exists" "already exists" "$out"
rm -rf $FAST/projects/p

# A folder in use by a live process is refused.
(cd $P/lines/c && exec sleep 60) & BUSY=$!
sleep 1
out=$(set -o pipefail; cd $T && l project move p fast 2>&1 | tee /dev/stderr); rc=$?
ok "a folder in use is refused" test "$rc" -ne 0
has "the refusal names the process" "in use" "$out"
kill $BUSY 2>/dev/null; wait $BUSY 2>/dev/null
no "the refused move leaves no copy" test -e $FAST/projects/p
ok "the refused move leaves the project on main" test -d $P/lines/main

# A volume that is not mounted is refused by name.
l volume add slow "$SLOW" >/dev/null
mkdir -p $SLOW/projects/p
out=$(set -o pipefail; cd $T && l project move p slow 2>&1 | tee /dev/stderr); rc=$?
ok "a target with a folder of the same name on another volume is refused" test "$rc" -ne 0
rm -rf $SLOW/projects/p
unmount slow
out=$(set -o pipefail; cd $T && l project move p slow 2>&1 | tee /dev/stderr); rc=$?
ok "a volume that is not mounted is refused" test "$rc" -ne 0
has "the refusal names the volume" "volume 'slow'" "$out"
remount slow
ok "the volume is usable again" l volume list

# Space before the move (the copy must keep the sharing).
NS=$("$LAYR" __project-space $P | json "d['need']")

# The move.
out=$(set -o pipefail; cd $T && l project move p fast 2>&1 | tee /dev/stderr); rc=$?
eq "the move succeeds" 0 "$rc"
has "the move reports the volumes" "from volume main to fast" "$out"
ok "the old folder is gone" test ! -e $P
eq "the record names fast" "fast" "$(record_of p)"
ok "the journal is gone" test ! -e $LAYR_ROOT/project-moves/p.json
F=$FAST/projects/p
ok "the project is on fast" test -d $F/lines/main
cd $F/lines/main || exit 1
out=$(l status --short 2>&1); rc=$?
eq "status works on fast" 0 "$rc"
has "log shows the saves" "save 8" "$(l log 2>&1)"
has "line list shows the three lines" "c" "$(l line list 2>&1)"
echo "after" >> notes.txt
has "a save on fast" "after" "$(l save -a -qm 'after move' >/dev/null 2>&1; l log -n 1 2>&1)"
echo "more" >> $F/lines/a/notes.txt
has "diff works on fast" "+more" "$(cd $F/lines/a && l diff 2>&1)"
ok "line new works on fast" sh -c "cd $F/lines/main && $LAYR line new -q d >/dev/null"
eq "a new line on fast starts warm" "$(cat $F/lines/main/target/big.bin | cksum)" "$(cat $F/lines/d/target/big.bin | cksum)"
has "the overlay reads on fast" "target" "$(l overlay list 2>&1)"
out=$(cd $F/lines/b && l park pop 2>&1); rc=$?
eq "the parked change comes back on fast" "wip" "$(cat $F/lines/b/wip.txt 2>/dev/null)"
if [ -f $F/lines/main/.git/objects/info/alternates ]; then
    eq "the git objects name the new folder" "$FASTP/projects/p/git/objects" "$(cat $F/lines/main/.git/objects/info/alternates)"
fi
NF=$("$LAYR" __project-space $F | json "d['need']")
echo "space: source need $NS, target need $NF"
ok "the target keeps the sharing (under 1.5x the source)" test "$NF" -le "$((NS * 3 / 2))"
cd $T || exit 1

# A crash after the copy: the project stays on main; the next command rolls back; a rerun completes.
clone_project q || exit 1
Q=$LAYR_ROOT/projects/q
out=$(set -o pipefail; cd $T && LAYR_TEST_MOVE_EXIT_AFTER=copy "$LAYR" project move q fast 2>&1 | tee /dev/stderr); rc=$?
eq "a crash after the copy exits" 3 "$rc"
ok "the copy is on fast" test -d $FAST/projects/q
ok "the project folder is intact on main" test -d $Q/lines/main
ok "the journal names the move" test -e $LAYR_ROOT/project-moves/q.json
eq "the record does not name fast" "" "$(record_of q)"
out=$(cd $Q/lines/main && l status 2>&1); rc=$?
eq "the project works on main after the crash" 0 "$rc"
no "the next command removes the partial copy" test -e $FAST/projects/q
no "the next command removes the journal" test -e $LAYR_ROOT/project-moves/q.json
out=$(set -o pipefail; cd $T && l project move q fast 2>&1 | tee /dev/stderr); rc=$?
eq "a rerun completes the move" 0 "$rc"
no "the old folder of q is gone" test -e $Q
eq "the record names fast after the rerun" "fast" "$(record_of q)"
ok "q works on fast" sh -c "cd $FAST/projects/q/lines/main && $LAYR status >/dev/null 2>&1"

# A crash after the switch: the next command finishes the move (the old folder goes).
clone_project r || exit 1
Rd=$LAYR_ROOT/projects/r
out=$(set -o pipefail; cd $T && LAYR_TEST_MOVE_EXIT_AFTER=switch "$LAYR" project move r fast 2>&1 | tee /dev/stderr); rc=$?
eq "a crash after the switch exits" 3 "$rc"
eq "the record names fast after the switch" "fast" "$(record_of r)"
ok "the old folder is still there before the next command" test -d $Rd
out=$(cd $FAST/projects/r/lines/main && l status 2>&1); rc=$?
eq "the next command works on fast" 0 "$rc"
no "the next command removes the old folder" test -e $Rd
no "the next command removes the journal" test -e $LAYR_ROOT/project-moves/r.json

finish
