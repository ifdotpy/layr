#!/bin/bash
# ci: macos overlayfs
# Push roles (`remote.<name>.push required|optional|off`; a layr remote is required, a git remote optional). `layr
# push` without a name pushes to every remote that is not off, required ones first: a git remote gets the line of the
# command and the protected lines. An optional failure is a warning and the next push tries again; a required one
# fails the command.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main" || exit 1
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
git init -q --bare "$T/mirror.git"
l git remote add origin "$T/mirror.git" >/dev/null
l protect main >/dev/null
no "a push role is required, optional or off" l config remote.origin.push sometimes
l line new topic -q
cd "$P/topic" || exit 1
echo t > t.txt; l add t.txt >/dev/null; l save -qm topic
out=$(l push --json)
ok "the store goes first, as required, then the git remote, as optional" python3 -c '
import json,sys; r=json.loads(sys.argv[1])["remotes"]
assert [(x["name"],x["kind"],x["role"]) for x in r]==[("cloud","layr","required"),("origin","git","optional")], r
assert r[0]["pushed"]==["*"] and set(r[1]["pushed"])=={"main","topic"} and not r[1]["failed"], r' "$out"
ok "the line of the command is on the git remote" git --git-dir="$T/mirror.git" rev-parse --verify -q refs/heads/topic
ok "the protected main too" git --git-dir="$T/mirror.git" rev-parse --verify -q refs/heads/main

# The git remote goes away: the store push stands, the line waits.
mv "$T/mirror.git" "$T/mirror.away"
echo t2 >> t.txt; l save -qam topic2
err=$(l push -q 2>&1 >/dev/null)
ok "an optional failure does not fail the push" test $? = 0
has "it is reported" "warning: push to origin (topic) failed" "$err"
mv "$T/mirror.away" "$T/mirror.git"
cd "$P/main" || exit 1
ok "a push of the git remote alone" l push origin -q
eq "brings the waiting line of another folder" "$(cd "$P/topic" && git rev-parse HEAD)" "$(git --git-dir="$T/mirror.git" rev-parse refs/heads/topic)"

# Roles: required fails the command, off leaves the remote out.
l config remote.origin.push required >/dev/null
mv "$T/mirror.git" "$T/mirror.away"
no "a required failure fails the push" l push -q
l config remote.origin.push off >/dev/null
ok "an off remote is left out" l push -q
mv "$T/mirror.away" "$T/mirror.git"
ok "a list in queue.push is accepted" l config queue.push cloud,origin
# Options of one remote's own push (and words after --) are no remote names.
ok "one remote with its own options takes its own push" l push cloud -q
has "a command after -- is no remote: the remote's own push reads it" "--command is available only for peer remotes" "$(l push cloud --command -- true 2>&1)"
finish
