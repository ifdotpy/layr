#!/bin/bash
# ci: macos overlayfs
# Volumes (`layr volume`): projects on file systems other than the store root. Two more file
# systems: APFS images attached here as the user on macOS, loop file systems on Linux (CI runs as
# root). A wrong identity or an unmounted volume refuses only its own projects.
. "$(dirname "$0")/lib.sh"

if [ "$(uname -s)" = Darwin ]; then
    # Images and mount points live outside the test root: hdiutil cannot mount an image at a
    # folder inside another mounted image ("Permission denied").
    VOLS=$(mktemp -d "${TMPDIR:-/tmp}/layr-volumes.XXXXXX")
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
        mkdir -p "$mp" # the test root is already a file system of the build (ext4 or XFS)
        return 0
    fi
    new_fs "$mp" 2G
}

# unmount <name> and remount <name>. On the OverlayFS build the folder cannot be moved away: its
# line mounts stay on the old path ("Device or resource busy"). There, a volume that is not
# mounted is a folder with no identity file, which is the same refusal.
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
    for name in fast slow spare; do
        hdiutil detach "$VOLS/$name" >/dev/null 2>&1 || hdiutil detach -force "$VOLS/$name" >/dev/null 2>&1 || true
    done
}

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

# mk <project> <folder holding its projects> [init options]: a project, and its main line
# unprotected in the macOS test mode (the lib wrapper cannot find a project on a volume).
mk() {
    local p=$1 base=$2; shift 2
    "$LAYR" init "$p" -q "$@" >/dev/null || return 1
    if [ "${LAYR_TEST_DIRECT_MAIN:-}" = 1 ]; then
        (cd "$base/projects/$p/lines/main" && "$LAYR" protect main --remove >/dev/null 2>&1) || return 1
    fi
}

# work <project> <folder holding its projects>: lines, saves, merge, caches, gc and du.
work() {
    local p=$1 base=$2 main=$2/projects/$1/lines/main
    cd "$main" || exit 1
    printf 'target/\n' > .gitignore; echo 'target regenerable' > .layrattributes; echo one > a.txt
    l add .gitignore .layrattributes a.txt; l save -qm "one"
    l line new -q topic >/dev/null
    cd "$base/projects/$p/lines/topic" || exit 1
    echo two > b.txt; l add b.txt; l save -qm "two"
    cd "$main" || exit 1
    l merge -q topic >/dev/null
    ok "$p: a merge brings the line into main" test -f "$main/b.txt"
    mkdir -p target/debug; echo bin > target/debug/x
    has "$p: save --caches names the glass" "target: overlay target." "$(l save --caches 2>/dev/null)"
    l line new -q warm >/dev/null
    eq "$p: a new line starts warm" "bin" "$(cat "$base/projects/$p/lines/warm/target/debug/x")"
    eq "$p: gc check is clean" 0 "$(l gc >/dev/null 2>&1; echo $?)"
    l gc --repair -q
    eq "$p: gc --repair keeps the project" 0 "$(l gc -q >/dev/null 2>&1; echo $?)"
    has "$p: du lists its lines" "main" "$(l du)"
}

sample $T/src
l init plain --from $T/src -q >/dev/null
no "a store without volumes has no volume file" test -e "$LAYR_ROOT/volumes.json"
eq "a store without volumes lists main only" "main" "$(l volume list --json | json "','.join(v['volume'] for v in d)")"

new_volume fast || { echo "SKIP volumes: this host cannot make a second file system"; finish; exit 0; }
new_volume slow || { echo "SKIP volumes: this host cannot make a third file system"; finish; exit 0; }
new_volume spare || { echo "SKIP volumes: this host cannot make a fourth file system"; finish; exit 0; }
FAST=$VOLS/fast SLOW=$VOLS/slow SPARE=$VOLS/spare
FASTP=$(cd "$VOLS" && pwd -P)/fast # volume add stores the resolved path

# Add, list, default, remove.
l volume add fast "$FAST" >/dev/null
l volume add slow "$SLOW" >/dev/null
eq "the identity file names this store and volume" "fast" "$(json "d['volume']" < "$FAST/layr-volume.json")"
no "a volume twice is refused" l volume add fast "$FAST"
mkdir -p "$LAYR_ROOT/inside" "$T/busy" && echo x > "$T/busy/file"
no "a path inside the store is refused" l volume add spare "$LAYR_ROOT/inside"
no "a folder with content is refused" l volume add spare "$T/busy"
no "a missing folder is refused" l volume add spare "$T/not-there"
eq "volume list names every volume" "main,fast,slow" "$(l volume list --json | json "','.join(v['volume'] for v in d)")"
eq "the first volume is the default" "main" "$(l volume list --json | json "[v['volume'] for v in d if v['default']][0]")"
l volume default fast >/dev/null
eq "volume default sets the default" "fast" "$(l volume list --json | json "[v['volume'] for v in d if v['default']][0]")"

# Init on a volume: the default one, an explicit one, and main.
mk web "$FAST"
mk archive "$SLOW" --volume slow
mk api "$LAYR_ROOT" --volume main
ok "init without --volume uses the default volume" test -d "$FAST/projects/web/lines/main"
no "the default volume gets no copy on main" test -e "$LAYR_ROOT/projects/web"
ok "init --volume puts the project on that volume" test -d "$SLOW/projects/archive/lines/main"
no "the other volume gets no copy" test -e "$FAST/projects/archive"
ok "init --volume main keeps the project on main" test -d "$LAYR_ROOT/projects/api/lines/main"
no "a project name that exists on a volume is refused" l init web --volume slow

# Work on each volume.
work web "$FAST"
work archive "$SLOW"
work api "$LAYR_ROOT"
has "du --volumes lists each volume" "fast" "$(l du --volumes)"
has "du --volumes shows the slow volume" "slow" "$(l du --volumes)"

# A wrong identity refuses its projects only.
cp "$FAST/layr-volume.json" "$T/fast-identity.json"
printf '{"format":1,"store":"someone-else","volume":"fast"}' > "$FAST/layr-volume.json"
out=$(cd "$FAST/projects/web/lines/main" && l status 2>&1); rc=$?
ok "a wrong identity refuses its project" test "$rc" -ne 0
has "the refusal names the volume" "volume 'fast'" "$out"
has "the refusal names the path" "$FASTP" "$out"
out=$(cd "$LAYR_ROOT/projects/api/lines/main" && l status 2>&1); rc=$?
eq "a project on another volume still works" 0 "$rc"
before=$(ls "$FAST/projects/web/states" | wc -l | tr -d ' ')
out=$(l gc --all-projects 2>&1); rc=$?
eq "gc --all-projects still exits 0" 0 "$rc"
has "gc names the skipped project" "skipped project web" "$out"
eq "gc deletes nothing for the refused volume" "$before" "$(ls "$FAST/projects/web/states" | wc -l | tr -d ' ')"
cp "$T/fast-identity.json" "$FAST/layr-volume.json"
eq "the volume works again with its identity" 0 "$(cd "$FAST/projects/web/lines/main" && l status >/dev/null 2>&1; echo $?)"

# An unmounted volume refuses its projects; the others keep working.
cd "$T"
unmount fast
out=$(LAYR_PROJECT=web l status 2>&1); rc=$?
ok "an unmounted volume refuses its project" test "$rc" -ne 0
has "the unmounted refusal names the volume" "volume 'fast'" "$out"
has "the unmounted refusal names the path" "$FASTP" "$out"
has "the volume list shows it missing" "missing" "$(l volume list)"
eq "another project works while the volume is gone" 0 "$(cd "$LAYR_ROOT/projects/api/lines/main" && l status >/dev/null 2>&1; echo $?)"
eq "the archive works while the volume is gone" 0 "$(cd "$SLOW/projects/archive/lines/main" && l status >/dev/null 2>&1; echo $?)"
has "gc skips the unmounted volume" "skipped project web" "$(l gc --all-projects 2>&1)"
has "du --store warns about the unmounted volume" "volume 'fast'" "$(cd "$LAYR_ROOT/projects/api/lines/main" && l du --store 2>&1)"
remount fast
eq "the project works after the volume is back" 0 "$(cd "$FAST/projects/web/lines/main" && l status >/dev/null 2>&1; echo $?)"

# Remove: a volume with a project, the default volume, and an empty volume.
l volume default main >/dev/null
no "a volume that holds a project is not removed" l volume remove fast
no "a volume with a project is not removed (second)" l volume remove slow
l volume add spare "$SPARE" >/dev/null
l volume remove spare >/dev/null
no "a removed volume is gone" bash -c "$LAYR volume list --json | grep -q spare"
ok "the default volume is not removed" bash -c "! $LAYR volume remove main >/dev/null 2>&1"

finish
