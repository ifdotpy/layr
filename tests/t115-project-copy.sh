#!/bin/bash
# ci: macos overlayfs
# Copy a whole project to another file system (#173 part C): the copy verifies, keeps the sharing
# between its states (APFS clones, btrfs send with parents), detects a changed file, and a copy
# that was interrupted is finished by the next call. The target must be empty and on another file
# system. macOS: a second APFS image; Linux: a second btrfs loop file system.
. "$(dirname "$0")/lib.sh"

D=$T/dst-fs
# Attach an image without a browser window; print its mount point.
attach_image() { # attach_image <image>
    hdiutil attach "$1" -nobrowse -plist | python3 -c 'import plistlib,sys
print([e["mount-point"] for e in plistlib.loads(sys.stdin.buffer.read())["system-entities"] if "mount-point" in e][0])'
}
MOUNTED=""
if [ "$(uname -s)" = Darwin ]; then
    # The images are attached and detached by this test only.
    detach_images() {
        for mp in $MOUNTED; do hdiutil detach "$mp" -force >/dev/null 2>&1 || true; done
    }
    trap 'detach_images; cleanup' EXIT
    hdiutil create -size 1g -fs APFS -volname "LayrCopy$$" -type SPARSE -o "$T/dst" >/dev/null 2>&1 || exit 1
    D=$(attach_image "$T/dst.sparseimage") || exit 1
    MOUNTED="$D"
elif [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    # OverlayFS: a second file system of the same type as the test root, mounted by this test.
    type=$(findmnt -n -o FSTYPE -T "$T")
    truncate -s 1G "$T/dst.img"
    if [ "$type" = xfs ]; then mkfs.xfs -q -f "$T/dst.img"; else mkfs.ext4 -q -F "$T/dst.img"; fi
    mkdir -p "$D"
    mount -o loop "$T/dst.img" "$D" || exit 1
    mount --make-private "$D" || exit 1
else
    new_fs "$D" 2G
fi

# The source project: a git bridge, 30 saves (thin layers on APFS), 3 lines and a 50 MB cache.
R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null
sample $T/src; cp -R $T/src/. $U/
printf 'target regenerable\n' > $U/.layrattributes
(cd $U && git add -A && git commit -qm "upstream 1" && git push -q origin main)
l git clone $R p -q >/dev/null
P=$LAYR_ROOT/projects/p
cd $P/lines/main || exit 1
for i in $(seq 1 30); do echo "save $i" >> notes.txt; l save -a -qm "save $i" >/dev/null; done
l line new -q w1 >/dev/null
l line new -q w2 >/dev/null
cd $P/lines/w1 || exit 1
echo "in w1" >> notes.txt
l save -a -qm "w1 work" >/dev/null
cd $P/lines/main || exit 1
mkdir -p target && head -c 52428800 /dev/urandom > target/big.bin
l save --caches -q >/dev/null
ok "the 50 MB cache is an overlay" l overlay list

j() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

# A check that prints the output of its command when it fails.
check() { # check <description> <command...>: the output is printed (CI logs keep the timings)
    local d=$1 out rc
    shift
    out=$("$@" 2>&1); rc=$?
    echo "  output: $out"
    if [ "$rc" -eq 0 ]; then PASS=$((PASS + 1)); echo "PASS $d"; else FAIL=$((FAIL + 1)); echo "FAIL $d"; fi
}

# Refusals change nothing.
mkdir -p $D/taken && echo keep > $D/taken/file
no "a target that is not empty is refused" "$LAYR" __project-copy $P $D/taken
eq "the refused target keeps its file" keep "$(cat $D/taken/file)"
no "a target on the same file system is refused" "$LAYR" __project-copy $P $LAYR_ROOT/copy-same
no "a target inside the source is refused" "$LAYR" __project-copy $P $P/copy-inside
no "a missing source is refused" "$LAYR" __project-copy $T/missing $D/none
no "a missing parent of the target is refused" "$LAYR" __project-copy $P $D/no/such/dir
no "the refusals leave no target" test -e $D/none
rm -rf $D/taken

# A copy with too little space on the target is refused.
if [ "$(uname -s)" = Darwin ]; then
    hdiutil create -size 40m -fs APFS -volname "LayrSmall$$" -type SPARSE -o "$T/small" >/dev/null 2>&1 || exit 1
    SMALL=$(attach_image "$T/small.sparseimage") || exit 1
    MOUNTED="$MOUNTED $SMALL"
    out=$("$LAYR" __project-copy $P $SMALL/p 2>&1); rc=$?
    eq "too little space is refused" 1 "$rc"
    has "the refusal names the space" "bytes are free" "$out"
    no "the refused copy leaves no target" test -e $SMALL/p
    hdiutil detach "$SMALL" -force >/dev/null 2>&1 || true
    MOUNTED=$(echo "$MOUNTED" | sed "s| $SMALL||")
else
    echo "SKIP too little space (checked on macOS only)"
fi

# Owners: a root copy keeps the owner and group of each file (the check needs root).
OWNED=0
if [ "$(id -u)" = 0 ]; then
    OWNED=1
    chown 1234:4321 $P/lines/w2/notes.txt
fi

# The copy and its verification (copy_project verifies the copy too).
check "copy the project" "$LAYR" __project-copy $P $D/p
no "the copy has no unfinished marker" test -e $D/p/.layr-copy-in-progress
check "verify the copy" "$LAYR" __project-verify $P $D/p
no "the source has no temporary snapshots" test -e $P/meta/.layr-copy-tmp
eq "the source status is clean" "" "$(cd $P/lines/main && l status --short)"

# Sharing: the copy occupies about what the source occupies, far less than a flattened copy.
S=$("$LAYR" __project-space $P); C=$("$LAYR" __project-space $D/p)
NS=$(echo "$S" | j need); TS=$(echo "$S" | j total); NC=$(echo "$C" | j need)
echo "space: source need $NS total $TS, copy need $NC"
ok "the copy keeps the sharing (under 1.5x the source)" test "$NC" -le "$((NS * 3 / 2))"
ok "the copy is far below a flattened copy" test "$((NC * 2))" -lt "$TS"

if [ "$OWNED" = 1 ]; then
    eq "the copy keeps the owner" "1234:4321" "$(stat -c %u:%g $D/p/lines/w2/notes.txt)"
    chown 999:999 $D/p/lines/w2/notes.txt
    no "a changed owner fails the verification" "$LAYR" __project-verify $P $D/p
    chown 1234:4321 $D/p/lines/w2/notes.txt
    ok "the restored owner verifies" "$LAYR" __project-verify $P $D/p
else
    echo "SKIP owners kept (needs root)"
fi

# A changed file makes the verification fail.
cp $D/p/lines/w1/notes.txt $T/notes.good
echo "corrupt" >> $D/p/lines/w1/notes.txt
no "a changed file fails the verification" "$LAYR" __project-verify $P $D/p
cp $T/notes.good $D/p/lines/w1/notes.txt
ok "the restored copy verifies" "$LAYR" __project-verify $P $D/p

# An interrupted copy leaves a marked target; the next call finishes it.
out=$(LAYR_TEST_COPY_EXIT_AFTER=5 "$LAYR" __project-copy $P $D/killed 2>&1); rc=$?
eq "the interrupted copy exits with an error" 3 "$rc"
ok "the interrupted target is marked" test -e $D/killed/.layr-copy-in-progress
no "the interrupted copy does not verify" "$LAYR" __project-verify $P $D/killed
check "the next call finishes the copy" "$LAYR" __project-copy $P $D/killed
no "the finished copy has no marker" test -e $D/killed/.layr-copy-in-progress
check "the finished copy verifies" "$LAYR" __project-verify $P $D/killed

finish
