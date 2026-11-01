#!/bin/bash
# Eviction: gc removes the tree of a state that a store remote holds and keeps its record. The
# next command that reads the tree fetches it back. Offline, the command names the remote that
# holds it. A base of a line's layer chain and a state with unsaved work stay on disk.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
P=$LAYR_ROOT/projects/project
cd "$P/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
echo first >> notes.txt; l save -a -m first -q
s1=$(l rev-parse head)
echo second >> notes.txt; l save -a -m second -q
s2=$(l rev-parse head)
echo third >> notes.txt; l save -a -m third -q
s3=$(l rev-parse head)
# A line that is deleted after its work went out: its final state stays in the undo window.
l line new -q tmp >/dev/null
cd "$P/lines/tmp"
echo temporary >> notes.txt; l save -a -m temporary -q
t1=$(l rev-parse head)
# A line with unsaved work after the push: its automatic state is not in the store.
cd "$P/lines/main"
l line new -q w >/dev/null
cd "$P/lines/w"
echo unsaved >> notes.txt; l save -q
u=$(l rev-parse 'head@{0}')
cd "$P/lines/main"
ok "push the history to the store" l push archive -q
ok "delete the temporary line" l line delete -D tmp -q

# A state folder is named by the UUID that ids::display maps to the id: bytes 10..16, 6..10 and 0..6.
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
on_disk() { [ -e "$P/states/$(dashed "$1")" ] || [ -e "$P/deltas/$(dashed "$1")" ]; }
trees() { find "$P/states" "$P/deltas" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l | tr -d ' '; }
before=$(trees)

ok "gc runs" l gc -q
ok "gc removes trees" test "$(trees)" -lt "$before"
no "a deleted line's final state is evicted" on_disk "$t1"
ok "the head keeps its tree" on_disk "$s3"
if [ "$(uname -s)" = Darwin ]; then
    ok "a base of the head's layer chain stays" on_disk "$s1"
else
    no "a pushed history state is evicted" on_disk "$s1"
fi
ok "the automatic state with unsaved work keeps its tree" on_disk "$u"
ok "fsck is clean after eviction" l fsck

has "show reads an evicted state from the store" temporary "$(l show "$t1:notes.txt")"
ok "the fetched state is on disk again" on_disk "$t1"
has "diff reads an evicted state" temporary "$(l diff "$s3" "$t1")"
ok "line new starts from a state fetched back" l line new -q x "$t1"
ok "gc --all-projects runs" l gc --all-projects -q
l line delete -D x -q >/dev/null

# Offline: the copy is on the store remote, which cannot be reached.
ok "gc evicts again" l gc -q
no "the evicted state is not on disk" on_disk "$t1"
l config cloud.remote.archive.url "file://$T/missing" >/dev/null
no "reading the evicted state offline fails" l show "$t1:notes.txt"
has "the offline error names the remote" "archive" "$(l show "$t1:notes.txt" 2>&1 || true)"
has "the offline error says the state is not on this machine" "not on this machine" "$(l show "$t1:notes.txt" 2>&1 || true)"
l config cloud.remote.archive.url "file://$T/archive" >/dev/null
has "the state reads again when the remote is back" temporary "$(l show "$t1:notes.txt")"
finish
