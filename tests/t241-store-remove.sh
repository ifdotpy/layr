#!/bin/bash
# ci: macos
# `layr store remove <path>` deletes a whole store folder, which `rm -rf` cannot (sealed entries). It refuses a folder
# that is not a layr store, the store of this shell without --force, and a store that a layr process uses. It unmounts
# what is mounted below the store and names data of the store that it keeps outside the folder.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
MNT=
trap '[ -z "$MNT" ] || hdiutil detach -force "$MNT" >/dev/null 2>&1; cleanup' EXIT
sample "$T/src"

# new_store <folder>: a store with a project, a saved state and a second line.
new_store() {
    LAYR_ROOT=$1 l init p --from "$T/src" -q >/dev/null || return 1
    (cd "$1/projects/p/lines/main" && LAYR_ROOT=$1 l line new -q topic) || return 1
    (cd "$1/projects/p/lines/topic" && echo more > more.txt && LAYR_ROOT=$1 l add more.txt && LAYR_ROOT=$1 l save -qm more) || return 1
}

ok 'a store' new_store "$T/a"
no 'rm -rf cannot delete a store' rm -rf "$T/a"
# rm deleted the marker and the files it could; sealed states stay.
ok 'sealed states of the store are still there' test -d "$T/a/projects/p/states"
no 'the store of this shell is refused' env LAYR_ROOT="$T/a" "$LAYR" store remove "$T/a"
ok 'a refused store keeps its files' test -d "$T/a/projects/p/states"
OUTPUT=$(l store remove "$T/a" 2>&1)
eq 'store remove deletes a store that rm -rf left half deleted' "Removed store $T/a" "$OUTPUT"
no 'the folder is gone' test -e "$T/a"

ok 'a store of this shell' new_store "$T/b"
ok 'store remove --force deletes the store of this shell' env LAYR_ROOT="$T/b" "$LAYR" store remove --force -q "$T/b"
no 'the forced folder is gone' test -e "$T/b"

mkdir -p "$T/plain/sub"; echo keep > "$T/plain/sub/file.txt"
no 'a folder that is not a store is refused' l store remove --force "$T/plain"
has 'the refusal names the marker' 'is not a layr store' "$(l store remove "$T/plain" 2>&1)"
eq 'a refused folder keeps its files' keep "$(cat "$T/plain/sub/file.txt")"
mkdir "$T/empty"
no 'an empty folder is refused' l store remove "$T/empty"
ok 'the empty folder stays' test -d "$T/empty"
no 'a missing folder fails' l store remove "$T/missing"
ok 'a store for a link' new_store "$T/c"
ln -s "$T/c" "$T/link"
no 'a link to a store is refused' l store remove "$T/link"
ok 'the store behind the link stays' test -f "$T/c/store-version.json"
no 'a usage error fails' l store delete "$T/c"

# A layr process that has the store open holds a shared lock on .layr-version.lock.
python3 -c '
import fcntl, sys, time
f = open(sys.argv[1]); fcntl.flock(f, fcntl.LOCK_SH); open(sys.argv[2], "w").close(); time.sleep(60)' \
    "$T/c/.layr-version.lock" "$T/locked" &
HOLDER=$!
for _ in $(seq 1 100); do [ -e "$T/locked" ] && break; sleep 0.05; done
has 'a store in use is refused' 'uses the store' "$(l store remove --force "$T/c" 2>&1)"
ok 'a store in use keeps its files' test -f "$T/c/store-version.json"
kill "$HOLDER"; wait "$HOLDER" 2>/dev/null
ok 'the store goes when no process uses it' l store remove -q "$T/c"
no 'the released folder is gone' test -e "$T/c"

# A moved line folder is outside the store: the command keeps it and names it.
ok 'a store with a moved line' new_store "$T/d"
(cd "$T/d/projects/p/lines/main" && LAYR_ROOT=$T/d l line move topic "$T/moved-topic" >/dev/null)
ok 'the line folder moved' test -f "$T/moved-topic/more.txt"
OUTPUT=$(l store remove "$T/d" 2>&1)
has 'store remove names the moved line folder it keeps' "kept a moved line folder outside the store: $T/moved-topic" "$OUTPUT"
no 'the store with a moved line is gone' test -e "$T/d"

# A file system mounted below the store is unmounted first.
if [ "$(uname -s)" = Darwin ] && hdiutil create -quiet -size 2m -fs HFS+ -volname layrtest "$T/disk.dmg" >/dev/null 2>&1; then
    ok 'a store with a mount' new_store "$T/e"
    MNT=$T/e/projects/p/mounted
    mkdir "$MNT"
    if hdiutil attach -quiet -nobrowse -mountpoint "$MNT" "$T/disk.dmg" >/dev/null 2>&1; then
        echo data > "$MNT/file.txt"
        ok 'store remove unmounts and deletes' l store remove -q "$T/e"
        no 'the store with a mount is gone' test -e "$T/e"
        MNT=
        no 'nothing stays mounted below it' sh -c "mount | grep -q ' on $T/e/'"
    else
        echo "SKIP hdiutil attach failed: the mount case did not run"
    fi
else
    echo "SKIP no disk image tool: the mount case did not run"
fi
finish
