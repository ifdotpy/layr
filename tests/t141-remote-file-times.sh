#!/bin/bash
# ci: macos overlayfs
# A layr store remote keeps the modification time of each file (`times` of a signed tree): a clone in
# another store restores the files and the cache folders with their times, so build tools that compare
# times (cargo) find a restored cache fresh. One user: the clone needs no other owner.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
touch -t 202001020304.05 "$T/src/a.txt" 2>/dev/null || { echo x > "$T/src/a.txt"; touch -t 202001020304.05 "$T/src/a.txt"; }
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target/debug
echo old > target/debug/dep; touch -t 202101010000.00 target/debug/dep
echo new > target/debug/out; touch -t 202101010000.01 target/debug/out
l save --glasses -q
mtime() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }

l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
ok "push to the store" l push archive -q
mkdir -p "$T/b"
env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines
eq "a file of the clone keeps its time" "$(mtime "$P/main/a.txt")" "$(mtime "$Q/main/a.txt")"
(cd "$Q/main" && env LAYR_ROOT="$T/b/store" "$LAYR" line new far main >/dev/null 2>&1)
eq "a cache file of a line in the clone keeps its time" "$(mtime "$P/main/target/debug/dep")" "$(mtime "$Q/far/target/debug/dep")"
eq "and the newer file stays newer" "$(mtime "$P/main/target/debug/out")" "$(mtime "$Q/far/target/debug/out")"
finish
