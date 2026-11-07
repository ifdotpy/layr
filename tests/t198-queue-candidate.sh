#!/bin/bash
# ci: macos
# A queued merge that is not a fast-forward and waits for its checks keeps its candidate (`merge.candidate`): the
# next queue pass takes the same state while the target head is the same, so runners check one state, and the line
# moves to exactly that state when its checks pass. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
here=$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/macos/')-$(uname -m | sed 's/arm64/aarch64/')
other=linux-x86_64; [ "$here" = linux-x86_64 ] && other=linux-aarch64
l protect main --remove >/dev/null 2>&1 || true
l line new -q topic
cd ../topic || exit 1
echo one > one.txt; l add one.txt; l save -qm one
cd ../main || exit 1
echo two > two.txt; l add two.txt; l save -qm two
l protect main --check gate >/dev/null
l config check.gate "echo gate" >/dev/null
l config checks.platforms "{\"gate\":[\"$here\",\"$other\"]}" >/dev/null
BASE=$(l rev-parse head)
l queue add topic >/dev/null
candidates() { sqlite3 "$P/meta/db/layr.sqlite" "select count(*) from records where op='merge.candidate'"; }
candidate() { sqlite3 "$P/meta/db/layr.sqlite" "select json_extract(raw,'\$.r.states[0].id') from records where op='merge.candidate' order by time desc limit 1"; }
has "the first pass waits for the other platform" "has no passing run on $other" "$(l queue run --into main 2>&1)"
eq "it recorded one candidate" 1 "$(candidates)"
first=$(candidate)
has "the second pass waits too" "has no passing run on $other" "$(l queue run --into main 2>&1)"
eq "and takes the same candidate" 1 "$(candidates)"
eq "main stays where it was" "$BASE" "$(l rev-parse head)"
l config checks.platforms "{\"gate\":\"$here\"}" >/dev/null
ok "with only this platform the entry merges" l queue run --into main
eq "main is exactly the recorded candidate" "$first" "$(sqlite3 "$P/meta/db/layr.sqlite" "select head from lines where name='main'")"
eq "both files are on main" "one two" "$(cat one.txt two.txt | tr '\n' ' ' | sed 's/ $//')"
finish
