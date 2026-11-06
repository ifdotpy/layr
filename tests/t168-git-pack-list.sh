#!/bin/bash
# ci: macos
# A Git pack of a store remote whose commit list names commits that the pack does not hold (an older layr listed
# the commits that its history exclusion left out) does not stop a clone: the commits are checked after all packs
# are in, and a commit that no pack holds is reported, not fatal. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
echo one > a.txt; git add -A; git commit -qm c1; git push -q origin main
l git clone $R it -q >/dev/null
M=$LAYR_ROOT/projects/it/lines/main
cd $M || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null 2>&1
ok "the first push writes a Git pack" l push archive -q
cd $U; echo two > a.txt; git commit -qam c2; git push -q origin main
cd $M && l git fetch origin -q >/dev/null 2>&1 && l merge origin/main -q >/dev/null 2>&1
ok "the second push writes another" l push archive -q
lists=$(ls "$T"/archive/git/*.commits.age 2>/dev/null)
eq "two packs" 2 "$(echo "$lists" | wc -l | tr -d ' ')"
# The second list names the commits of the first; the first list goes, so no pack holds them by its list.
first=$(echo "$lists" | head -1); second=$(echo "$lists" | tail -1)
cp "$first" "$second.tmp" && mv "$second.tmp" "$second" && rm -f "$first"
mkdir -p "$T/b"
out=$(env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper" "$LAYR" clone "file://$T/archive" it 2>&1); rc=$?
eq "the clone succeeds" 0 "$rc"
no "it does not fail on the pack" grep -q "does not hold all its commits" <<<"$out"
finish
