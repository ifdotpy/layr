#!/bin/bash
# Eviction keeps a state whose tree the store format cannot restore exactly, such as one with an
# extended attribute. A plain state is still evicted. (Hard links and empty folders do not reach a
# state through save on APFS; `format_gap_tests` in src/store_remote/writer.rs covers them.)
. "$(dirname "$0")/lib.sh"
set -e
set_xattr() {
    if [ "$(uname -s)" = Darwin ]; then xattr -w user.layr kept "$1"; else setfattr -n user.layr -v kept "$1"; fi
}
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

# A line whose only state is the one named here; the line is deleted so that nothing else keeps it.
make_state() { # make_state <name> <setup command...>
    local name=$1; shift
    l line new -q "$name" >/dev/null
    cd "$P/lines/$name"
    "$@"
    echo "$name" >> notes.txt
    l save -a -m "$name" -q
    STATE=$(l rev-parse head)
    l push archive -q
    cd "$P/lines/main"
    l line delete -D "$name" -q
}
nothing() { :; }
attribute() { set_xattr notes.txt; }

make_state plain nothing
plain=$STATE
make_state attr attribute
attr_state=$STATE
cd "$P/lines/main"

ok "gc with gc.evict on runs" l gc -q
no "a plain state is evicted" on_disk "$plain"
ok "a state with an extended attribute stays on disk" on_disk "$attr_state"
finish
