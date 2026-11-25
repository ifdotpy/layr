#!/bin/bash
# ci: macos
# Git packs stay in the root namespace of a store remote through key epochs, so a pack written before a rotation is
# encrypted to the older epoch. A push and a clone after the rotation read it with the older key. On 2026-11-24 an
# untrust made every fetch of project layr fail with "No matching keys found". One user.
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
ok "a rotation makes a new key epoch" l remote rotate archive
cd $U; echo two > a.txt; git commit -qam c2; git push -q origin main
cd $M && l git fetch origin -q >/dev/null 2>&1 && l merge origin/main -q >/dev/null 2>&1
ok "a push after the rotation reads the older pack" l push archive -q
eq "two packs" 2 "$(ls "$T"/archive/git/*.commits.age 2>/dev/null | wc -l | tr -d ' ')"
mkdir -p "$T/b"
out=$(env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper" "$LAYR" clone "file://$T/archive" it 2>&1); rc=$?
eq "a clone reads the packs of both epochs" 0 "$rc"
no "no key error" grep -q "No matching keys" <<<"$out"
finish
