#!/bin/bash
# ci: macos overlayfs
# A virtual-line deletion refused after preparation keeps its upper data and discards its state.
# Like t199, this needs macOS and the installed, enabled LayrFS extension.
if [ "$(uname -s)" != Darwin ]; then
    echo "t257-line-delete-virtual-refusal.sh (needs macOS): 0 passed, 0 failed"
    exit 0
fi
if ! pluginkit -m -p com.apple.fskit.fsmodule 2>/dev/null | grep -q dev.layr.fshost.fs; then
    echo "t257-line-delete-virtual-refusal.sh (needs the LayrFS extension): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"
export LAYR_EXPERIMENTAL_VIEWS=1

unmount_views() {
    local mp
    for mp in $(mount | awk -v t="$T" '/\(layrfs,/ && index($3, t) == 1 { print $3 }'); do
        umount "$mp" 2>/dev/null || umount -f "$mp"
    done
}
trap 'unmount_views; cleanup' EXIT

state_ids() {
    python3 - "$P" <<'PY'
from pathlib import Path
import sys
project = Path(sys.argv[1])
print('\n'.join(sorted({p.name for kind in ('states', 'deltas')
                        if (project / kind).is_dir() for p in (project / kind).iterdir() if p.is_dir()})))
PY
}

sample "$T/src"
l init delview --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/delview
cd "$P/lines/main"
l line new -q virt main --view virtual || exit 1
V=$P/lines/virt
cd "$V"
printf 'saved\n' > saved.txt
l save -qm virtual || exit 1
printf 'keep the upper data\n' > unsaved.txt
cd "$P/lines/main"
upper=$(printf '%s\n' "$P"/meta/vlines/*/upper)
[ -d "$upper" ] || { echo 'FATAL: the virtual upper folder is missing'; exit 1; }

has "the refused deletion starts with a mounted target" "$V (layrfs" "$(mount)"
out=$(l line delete --discard virt 2>&1); rc=$?
eq "an unmerged virtual line is refused" 1 "$rc"
has "the early virtual refusal names the destination" "not fully merged into 'main'" "$out"
has "the early refusal keeps the mount" "$V (layrfs" "$(mount)"
has "the early refusal keeps the line" virt "$(l line list)"
eq "the early refusal keeps upper data" "keep the upper data" "$(cat "$upper/unsaved.txt")"

l merge -q virt || exit 1
state_ids > "$T/states.before"
barrier=$T/virtual-barrier
mkdir "$barrier"
LAYR_TEST_LINE_DELETE_BARRIER="$barrier" l line delete --discard virt >"$T/delete.out" 2>&1 &
pid=$!
for _ in $(seq 1 500); do
    [ -f "$barrier/ready" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.02
done
if [ ! -f "$barrier/ready" ]; then
    touch "$barrier/release"
    wait "$pid"
    cat "$T/delete.out"
    echo 'FATAL: virtual-line deletion did not reach the barrier'
    exit 1
fi
state_ids > "$T/states.prepared"
comm -13 "$T/states.before" "$T/states.prepared" > "$T/prepared"
no "virtual deletion publishes no native state before the barrier" test -s "$T/prepared"
find "$P/meta/work" -mindepth 2 -maxdepth 2 -type d -name tree > "$T/workspaces"
ok "virtual deletion captures its tree in a workspace" test -s "$T/workspaces"
l reset -q --hard head~1 || { touch "$barrier/release"; wait "$pid"; exit 1; }
touch "$barrier/release"
wait "$pid"; rc=$?

eq "a destination rewound during virtual deletion refuses" 1 "$rc"
has "the locked virtual refusal names the destination" "not fully merged into 'main'" "$(cat "$T/delete.out")"
has "the locked refusal keeps the virtual line" virt "$(l line list)"
eq "the locked refusal keeps upper data" "keep the upper data" "$(cat "$upper/unsaved.txt")"
no "our own unmount is not a folder replacement" grep -q "working folder .* changed" "$T/delete.out"
state_ids > "$T/states.refused"
ok "the refused deletion publishes no native state" cmp -s "$T/states.before" "$T/states.refused"
while IFS= read -r tree; do
    no "the refused deletion retires its capture workspace" test -e "$tree"
done < "$T/workspaces"
ok "the refused virtual line can be mounted again" l line show virt --virtual
eq "the remounted line still reads its upper data" "keep the upper data" "$(cat "$V/unsaved.txt")"
before=$(l line list virt --json) || exit 1
out=$(l line delete -D --discard virt --json); rc=$?
eq "the virtual line can still be explicitly discarded" 0 "$rc"
ok "virtual deletion returns the line JSON" python3 -c 'import json, sys; assert json.loads(sys.argv[1]) == [json.loads(sys.argv[2])]' "$before" "$out"

finish
