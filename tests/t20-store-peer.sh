#!/bin/bash
# A store restore retains the identity needed by incremental peer streams.
. "$(dirname "$0")/lib.sh"
set -e
check_sync() {
    local description=$1; shift
    local rc=0
    peer_exchange "$@" > "$T/peer.out" 2> "$T/peer.err" || rc=$?
    eq "$description" 0 "$rc"
    if [ "$rc" != 0 ]; then cat "$T/peer.out" "$T/peer.err"; fi
}
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
AR=$LAYR_ROOT
A=$AR/projects/project/lines/main
cd "$A"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
new_fs "$T/c" 4G
CR=$T/c/store
export LAYR_ROOT=$CR LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
l clone "file://$T/archive" project -q
unset LAYR_PAPER_KEY_FILE
C=$CR/projects/project/lines/main
cd "$C"
# A must trust C before its existing peer service accepts C's signed records.
l op export > "$T/c-records"
read -r cm ck < <(python3 - "$T/c-records" "$CR/machine/id" <<'PYKEY'
import json,sys
machine=open(sys.argv[2]).read().strip()
for line in open(sys.argv[1]):
    record=json.loads(line)['r']
    if record['machine']==machine and record['seq']==1:
        print(machine,record['data']['public_key'])
        break
PYKEY
)
env LAYR_ROOT="$AR" "$LAYR" --project project remote add clone ssh://test/project >/dev/null
env LAYR_ROOT="$AR" "$LAYR" --project project remote trust clone "$cm" --public-key "$ck" >/dev/null
check_sync "restored replica syncs with its original source" sync project -q -- env LAYR_ROOT="$AR" "$LAYR" serve-peer project
new_fs "$T/b" 4G
BR=$T/b/store
check_sync "restored replica sends full states to a new peer" sync project -q -- env LAYR_ROOT="$BR" "$LAYR" serve-peer project
# A adds a child. C must receive it using its restored parent.
export LAYR_ROOT=$AR
cd "$A"
echo from-A >> notes.txt
l commit -qam from-A
from_a=$(l rev-parse HEAD)
export LAYR_ROOT=$CR
cd "$C"
check_sync "restored parent receives an incremental child from A" sync project -q -- env LAYR_ROOT="$AR" "$LAYR" serve-peer project
has "received state contains A's bytes" from-A "$(l show "$from_a:notes.txt")"
# C adds a different child from the same restored parent.
echo from-C >> notes.txt
l commit -qam from-C
from_c=$(l rev-parse HEAD)
check_sync "C sends its child back to A" sync project -q -- env LAYR_ROOT="$AR" "$LAYR" serve-peer project
check_sync "C sends an incremental child to B" sync project -q -- env LAYR_ROOT="$BR" "$LAYR" serve-peer project
has "B receives C's exact state" from-C "$(env LAYR_ROOT="$BR" "$LAYR" --project project show "$from_c:notes.txt")"
# The return direction starts from a received parent on B.
env LAYR_ROOT="$BR" "$LAYR" --project project adopt main >/dev/null
export LAYR_ROOT=$BR
cd "$BR/projects/project/lines/main"
echo from-B >> notes.txt
l commit -qam from-B
from_b=$(l rev-parse HEAD)
export LAYR_ROOT=$CR
cd "$C"
check_sync "C receives B's child of a received parent" sync project -q -- env LAYR_ROOT="$BR" "$LAYR" serve-peer project
has "C receives B's exact state" from-B "$(l show "$from_b:notes.txt")"
ok "restored replica keeps valid signed records" l fsck
finish
