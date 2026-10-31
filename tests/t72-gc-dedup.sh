#!/bin/bash
# `layr gc` shares equal content between cache folders (docs/cache-overlays.md, "Deduplication"):
# APFS clones whole files, btrfs shares blocks with FIDEDUPERANGE. One user, so it runs on
# macOS and on Linux.
. "$(dirname "$0")/lib.sh"
[ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ] && { echo "t72-gc-dedup.sh: 0 passed, 0 failed (OverlayFS shares nothing)"; exit 0; }

mtime() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_mtime_ns)' "$1"; }
inode() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_ino)' "$1"; }

sample $T/src
printf 'target regenerable\nout2 regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true

# Two lines with the same build output. The second line rewrites a file as a new file with the
# same content, as a build tool does, and one file with other content.
l line new a main >/dev/null
cd $P/a
rm -rf target; mkdir -p target/debug
head -c 600000 /dev/urandom > target/debug/same.o
head -c 600000 /dev/urandom > target/debug/other.o
mkdir -p out2; head -c 600000 /dev/urandom > out2/x.o
l save --caches -q
l line new b a >/dev/null
cd $P/b
# Not cp or cat: GNU cp and cat clone the file on btrfs and APFS. dd reads and writes.
dd if=target/debug/same.o of=target/debug/same.tmp bs=65536 2>/dev/null
rm target/debug/same.o
mv target/debug/same.tmp target/debug/same.o
head -c 600000 /dev/urandom > target/debug/other.o
# Equal content in another cache overlay must stay apart from target/.
mkdir -p out2; dd if=target/debug/same.o of=out2/copy.o bs=65536 2>/dev/null
touch -t 202001020304.05 target/debug/same.o
chmod 640 target/debug/same.o
before_mtime=$(mtime target/debug/same.o)
before_inode=$(inode target/debug/same.o)
before_other=$(cksum < target/debug/other.o)

# 1. A dry run reports the bytes and changes nothing.
out=$(l gc -n)
has "dry run reports what it would share" "would share" "$out"
case "$out" in *"would share 0 B"*) FAIL=$((FAIL + 1)); echo "FAIL dry run counted no bytes: $out" ;; *) PASS=$((PASS + 1)); echo "PASS dry run counted bytes" ;; esac
eq "dry run keeps the inode" "$before_inode" "$(inode target/debug/same.o)"
eq "dry run keeps the mtime" "$before_mtime" "$(mtime target/debug/same.o)"

# 2. --no-dedup does not touch the files and prints no dedup line.
out=$(l gc --no-dedup)
case "$out" in *"shared"*) FAIL=$((FAIL + 1)); echo "FAIL --no-dedup prints a dedup line" ;; *) PASS=$((PASS + 1)); echo "PASS --no-dedup prints no dedup line" ;; esac
eq "--no-dedup keeps the inode" "$before_inode" "$(inode target/debug/same.o)"

# 3. gc shares the copy; content, mtime and mode stay; other content is untouched.
out=$(l gc)
has "gc reports the shared bytes" "shared " "$out"
has "only the file of its own overlay is shared" "in 1 cache file(s)" "$out"
has "gc reports the time" " s" "$out"
case "$out" in *"shared 0 B"*) FAIL=$((FAIL + 1)); echo "FAIL gc shared nothing: $out" ;; *) PASS=$((PASS + 1)); echo "PASS gc shared bytes" ;; esac
eq "the content is the same" "$(cksum < $P/a/target/debug/same.o)" "$(cksum < target/debug/same.o)"
eq "the mtime stays" "$before_mtime" "$(mtime target/debug/same.o)"
eq "the mode stays" 640 "$(stat_field mode target/debug/same.o)"
eq "a file with other content stays" "$before_other" "$(cksum < target/debug/other.o)"
if [ "$(uname -s)" = Darwin ]; then
    [ "$(inode target/debug/same.o)" != "$before_inode" ] && r=1 || r=0
    eq "APFS replaced the duplicate by a clone" 1 "$r"
fi
eq "no temporary file is left" "" "$(ls -a target/debug | grep layr-dedup)"

# 4. A second gc finds nothing new to share.
out=$(l gc)
has "a second gc shares nothing more" "shared 0 B" "$out"

finish
