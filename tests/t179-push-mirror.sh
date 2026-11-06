#!/bin/bash
# ci: macos overlayfs
# `remote.<store>.mirror <git remote>`: a push to the store also pushes the line of the command and the protected
# lines to the git remote. A git failure never fails the store push: it is reported, and the next push tries the line
# again.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
git init -q --bare "$T/mirror.git"
l git remote add origin "$T/mirror.git" >/dev/null
l config remote.cloud.mirror origin >/dev/null
l protect main >/dev/null
l line new topic -q
cd "$P/topic" || exit 1
echo t > t.txt; l add t.txt >/dev/null; l save -qm topic
out=$(l push cloud --json)
python3 -c 'import json,sys; m=json.loads(sys.argv[1])["mirror"]; assert m["remote"]=="origin" and "topic" in m["pushed"] and not m["failed"], m' "$out"
ok "the push reports the mirror" test $? = 0
ok "the line of the command is on the git remote" git --git-dir="$T/mirror.git" rev-parse --verify -q refs/heads/topic
ok "the protected main too" git --git-dir="$T/mirror.git" rev-parse --verify -q refs/heads/main

# The git remote goes away: the store push still works, the line waits.
mv "$T/mirror.git" "$T/mirror.away"
echo t2 >> t.txt; l save -qam topic2
err=$(l push cloud -q 2>&1 >/dev/null)
ok "the store push succeeds while the mirror fails" test $? = 0
has "the failure is reported" "mirror origin: line topic not pushed" "$err"
mv "$T/mirror.away" "$T/mirror.git"
cd "$P/main" || exit 1
l push cloud -q >/dev/null 2>&1
eq "topic on the git remote is the newest save" "$(cd "$P/topic" && git rev-parse HEAD 2>/dev/null)" "$(git --git-dir="$T/mirror.git" rev-parse refs/heads/topic)"
finish
