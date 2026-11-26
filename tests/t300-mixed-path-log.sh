#!/bin/bash
# ci: macos overlayfs
# A literal file query reads a Git-only parent without building a snapshot.
. "$(dirname "$0")/lib.sh"

git init -qb main "$T/up"
cd "$T/up"
echo same > inherited.txt
echo before > changed.txt
git add .
git commit -qm base
git branch sibling || exit 1
l git clone "$T/up" mixed -q >/dev/null || exit 1

echo after > changed.txt
git commit -qam imported
git switch -q sibling || exit 1
echo sibling > changed.txt
git commit -qam sibling || exit 1
git switch -q main || exit 1

project=$LAYR_ROOT/projects/mixed
cd "$project/lines/main"
# Require a complete head when GC compacts thin states.
l config thin.chain 0 || exit 1
l config undo.ops 0 || exit 1
l config import.trees 0 || exit 1
l git fetch -q origin || exit 1
read -r state base < <(l log -1 --json origin/main | python3 -I -c '
import json, sys
state = json.load(sys.stdin)
print(state["id"], state["parents"][0])
') || exit 1
l line new -q topic "$state" || exit 1
cd "$project/lines/topic"
if [ "$(uname -s)" = Darwin ]; then
    xattr -w user.note native inherited.txt || exit 1
else
    python3 -I -c 'import os; os.setxattr("inherited.txt", "user.note", b"native")' || exit 1
fi
echo native > extra
l add extra inherited.txt || exit 1
l save -qam native || exit 1
# APFS can save a thin child. Retire its layer before expiring the Git parent.
l gc --repair -q || exit 1

# Build the sibling to expire the parent's cached tree without reading that parent again.
# Main still owns the earlier base.
ok "the imported parent is built before eviction" state_available "$project" "$state"
l try -q origin/sibling -- true || exit 1
ok "the earlier imported base remains available" state_available "$project" "$base"
no "the imported parent has no snapshot" state_available "$project" "$state"
for path in inherited.txt changed.txt; do
    literal=$(LAYR_TRACE=1 l log --format=%s "$state..head" -- "$path" 2>"$T/trace")
    eq "$path literal query succeeds" 0 "$?"
    no "$path literal query avoids a Git snapshot build" grep -E 'build [[:xdigit:]]+ from git' "$T/trace"
    no "the imported parent remains absent after $path" state_available "$project" "$state"
    expected=
    [ "$path" != inherited.txt ] || expected=native
    eq "$path keeps metadata and content membership" "$expected" "$literal"
done

# Wildcard selection uses the full source comparison and restores the missing state.
full=$(l log --format=%s "$state..head" -- 'inherite[d].txt')
eq "full query succeeds" 0 "$?"
eq "literal and full selection agree on the xattr-only change" native "$full"
ok "the wildcard query restored the imported parent" state_available "$project" "$state"
finish
