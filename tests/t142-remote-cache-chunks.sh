#!/bin/bash
# ci: macos overlayfs
# A cache layer goes to a layr store remote as content-defined chunks in pack objects
# (docs/remote-format.md, "Chunks of cache layers"): a clone restores the cache with equal bytes and
# times, and a small change in a large build file uploads only the chunks around the change, also
# when the file moves to a new path, as cargo does with a rebuilt unit. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target/debug/deps
# 3 MiB that zstd cannot shrink, so the pack sizes show what went up.
python3 -c 'import os,sys; open(sys.argv[1],"wb").write(os.urandom(3 << 20))' target/debug/deps/libbig-1111111111111111.rlib
cp target/debug/deps/libbig-1111111111111111.rlib target/debug/deps/copy.rlib
echo small > target/debug/out
touch -t 202101010000.00 target/debug/out
l save --caches -q
mtime() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }
packs() { find "$T/archive" -path '*/chunks/*.pack.age' | sort; }
bytes() { python3 -c 'import os,sys; print(sum(os.path.getsize(p) for p in sys.argv[1:]))' "$@"; }

l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
ok "push the cache to the store" l push archive -q --caches
first=$(packs)
ok "the cache layer has chunk packs" test -n "$first"
eq "each pack has its index" "$(find "$T/archive" -path '*/chunks/*.index.age' | wc -l | tr -d ' ')" "$(echo "$first" | wc -l | tr -d ' ')"
size=$(bytes $first)
# The copy and the original share every chunk: one copy of 3 MiB of noise goes up, not two.
ok "equal files share chunks ($size bytes)" test "$size" -lt $((4 << 20))

mkdir -p "$T/b"
env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
Q=$T/b/store/projects/p/lines
(cd "$Q/main" && env LAYR_ROOT="$T/b/store" "$LAYR" line new far main >/dev/null 2>&1)
ok "a chunked file comes back with equal bytes" cmp "$P/main/target/debug/deps/libbig-1111111111111111.rlib" "$Q/far/target/debug/deps/libbig-1111111111111111.rlib"
ok "and its copy too" cmp "$P/main/target/debug/deps/copy.rlib" "$Q/far/target/debug/deps/copy.rlib"
eq "a chunked file keeps its time" "$(mtime "$P/main/target/debug/out")" "$(mtime "$Q/far/target/debug/out")"

# A rebuilt unit: a new hash in the name and 100 bytes inserted in the middle.
python3 - "$P/main/target/debug/deps" <<'EOF'
import os, sys
d = sys.argv[1]
old = os.path.join(d, "libbig-1111111111111111.rlib")
data = open(old, "rb").read()
middle = len(data) // 2
open(os.path.join(d, "libbig-2222222222222222.rlib"), "wb").write(data[:middle] + os.urandom(100) + data[middle:])
os.remove(old)
EOF
l save --caches -q
ok "push the changed cache" l push archive -q --caches
new=$(comm -13 <(echo "$first") <(packs))
added=$(bytes $new)
# Only the chunks around the insertion are new: at most a few 256 KiB chunks, never the 3 MiB file.
ok "a moved and changed file uploads only its new chunks ($added bytes)" test "$added" -lt $((1 << 20))
mkdir -p "$T/c"
env LAYR_ROOT="$T/c/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
R=$T/c/store/projects/p/lines
(cd "$R/main" && env LAYR_ROOT="$T/c/store" "$LAYR" line new far main >/dev/null 2>&1)
ok "the changed file comes back with equal bytes" cmp "$P/main/target/debug/deps/libbig-2222222222222222.rlib" "$R/far/target/debug/deps/libbig-2222222222222222.rlib"
ok "and the removed file is gone" test ! -e "$R/far/target/debug/deps/libbig-1111111111111111.rlib"
finish
