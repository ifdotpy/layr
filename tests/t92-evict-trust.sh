#!/bin/bash
# Eviction trusts a store copy only while the remote under the recorded name still has the recorded
# URL. A removed remote or a changed URL ends the guarantee: the state is not evicted.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
P=$LAYR_ROOT/projects/project
cd "$P/lines/main"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l config gc.evict on >/dev/null
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
on_disk() { [ -e "$P/states/$(dashed "$1")" ] || [ -e "$P/deltas/$(dashed "$1")" ]; }

# A line whose only state goes to the named remote. The line is deleted, so nothing else keeps it.
make_state() { # make_state <name> <remote>
    l line new -q "$1" >/dev/null
    cd "$P/lines/$1"
    echo "$1" >> notes.txt
    l save -a -m "$1" -q
    STATE=$(l rev-parse head)
    l push "$2" -q
    cd "$P/lines/main"
    l line delete -D "$1" -q
}

make_state control archive
control=$STATE
ok "gc evicts a copy of an unchanged remote" l gc -q
no "the state with an unchanged remote is evicted" on_disk "$control"

l remote add moved "file://$T/moved" --paper-key-out "$T/paper-moved" >/dev/null
make_state changed moved
changed=$STATE
l config cloud.remote.moved.url "file://$T/elsewhere" >/dev/null
ok "gc runs after the URL changed" l gc -q
ok "the state of a changed URL is not evicted" on_disk "$changed"
has "the state of a changed URL still reads from this machine" "changed" "$(l show "$changed:notes.txt")"

l remote add gone "file://$T/gone" --paper-key-out "$T/paper-gone" >/dev/null
make_state removed gone
removed=$STATE
l remote remove gone >/dev/null
ok "gc runs after the remote was removed" l gc -q
ok "the state of a removed remote is not evicted" on_disk "$removed"
has "the state of a removed remote still reads from this machine" "removed" "$(l show "$removed:notes.txt")"
finish
