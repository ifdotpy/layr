#!/bin/bash
# ci: macos
# `clone --heads` (docs/remote-format.md): a CI clone restores only the heads of the lines. The other states of the
# history stay on the store remote, and a command that reads one restores it from there. A plain clone restores the
# whole history, so it works offline.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
git init -q -b main "$T/src"
git -C "$T/src" add .
git -C "$T/src" commit -qm base
base_commit=$(git -C "$T/src" rev-parse HEAD)
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
for m in x y; do
  read id key <<<"$(env LAYR_ROOT="$T/$m/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
  l remote trust cloud "$id" --public-key "$key" >/dev/null
done
for i in 1 2 3 4 5; do echo $i > h$i.txt; l add h$i.txt >/dev/null; l save -qm h$i; done
old=$(l log --json 2>/dev/null | python3 -c 'import json,sys; lines=[json.loads(l) for l in sys.stdin]; print(lines[3]["id"])')
head_commit=$(git rev-parse HEAD)
l push cloud -q >/dev/null 2>&1
states() { ls "$T/$1/store/projects/p/states" 2>/dev/null | wc -l | tr -d ' '; }

xl() { env LAYR_ROOT="$T/x/store" "$LAYR" "$@"; }
out=$(xl clone "file://$T/archive" p --heads --overlay main 2>&1)
has "a --heads clone leaves the history on the remote" "restored on first read (--line, --overlay)" "$out"
ok "and has the head" test -f "$T/x/store/projects/p/lines/main/h5.txt"
eq "Git-folder creation leaves omitted native history states on the remote" 1 "$(states x)"
has "Git reports the omitted state rather than downloading history" ".git folder shows an older commit: state " "$out"
eq "Git HEAD falls back to the imported ancestor" "$base_commit" "$(git -C "$T/x/store/projects/p/lines/main" rev-parse HEAD)"
no "the fallback Git tree lacks the head's new files" git -C "$T/x/store/projects/p/lines/main" cat-file -e HEAD:h5.txt
(cd "$T/x/store/projects/p/lines/main" && xl line new old "$old" >/dev/null 2>&1)
ok "a new line from an older state restores it from the remote" test -f "$T/x/store/projects/p/lines/old/h1.txt"
no "with the files of that state" test -f "$T/x/store/projects/p/lines/old/h5.txt"

yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
yl clone "file://$T/archive" p --overlay main >/dev/null 2>&1
ok "a plain clone restores more states than the --heads clone did" test "$(states y)" -gt "$(( $(states x) - 1 ))"
eq "a plain clone creates Git HEAD at the current state" "$head_commit" "$(git -C "$T/y/store/projects/p/lines/main" rev-parse HEAD)"
(cd "$T/x/store/projects/p/lines/main" && xl git sync -q >/dev/null 2>&1)
eq "explicit Git export restores history and fixes Git HEAD" "$head_commit" "$(git -C "$T/x/store/projects/p/lines/main" rev-parse HEAD)"
ok "the repaired Git tree includes the head's files" git -C "$T/x/store/projects/p/lines/main" cat-file -e HEAD:h5.txt

# A push that published a state after a clone read the records: the clone sees a tree without its record. It takes
# the rest, says so, and the next fetch takes that state.
mkdir -p "$T/archive/states/01a1aaaa-0000-7000-8000-000000000009"
echo later > "$T/archive/states/01a1aaaa-0000-7000-8000-000000000009/deadbeef.tree.age"
zl() { env LAYR_ROOT="$T/z/store" "$LAYR" "$@"; }
read zid zkey <<<"$(zl remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$zid" --public-key "$zkey" >/dev/null; l push cloud -q >/dev/null 2>&1
out=$(zl clone "file://$T/archive" p --overlay main 2>&1); code=$?
eq "a tree whose record the clone did not read does not fail it" 0 "$code"
has "it says why it left the tree" "its record is not taken here yet" "$out"
finish
