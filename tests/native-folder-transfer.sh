#!/bin/bash
# Local bindings must never redirect a backup restore or a replica.
. "$(dirname "$0")/lib.sh"
sample "$T/source"
l init native --from "$T/source" -q >/dev/null || exit 1
(cd "$LAYR_ROOT/projects/native/lines/main" && l protect main --remove) || exit 1
l workspace attach native --path "$T/source" >/dev/null || exit 1
cd "$T/source" || exit 1
printf 'committed\n' > change
l add change && l commit -qm native || exit 1
printf 'local work\n' > unfinished
ok "back up an attached project" l backup -q "$T/backup"
ok "verify the attached project backup" l backup verify "$T/backup"
new_fs "$T/restored" 2G
ok "restore without the local binding" env LAYR_ROOT="$T/restored/store" "$LAYR" backup restore -q "$T/backup"
RESTORED="$T/restored/store/projects/native"
eq "restore has committed content" committed "$(cat "$RESTORED/lines/main/change")"
eq "restore has the uncommitted content" "local work" "$(cat "$RESTORED/lines/main/unfinished")"
no "restore excludes the source locator" test -e "$RESTORED/lines/main/.layr"
no "restore excludes the private binding" test -e "$RESTORED/meta/workspace.json"
eq "restore leaves the original work intact" "local work" "$(cat unfinished)"
ok "the original binding survives restore" test -f .layr/link.json
new_fs "$T/peer" 2G
ok "sync from an attached project" l sync -q -- env LAYR_ROOT="$T/peer/store" "$LAYR" serve-peer native
eq "the peer has the same head" "$(l rev-parse HEAD)" "$(env LAYR_ROOT="$T/peer/store" "$LAYR" --project native --line main rev-parse HEAD)"
no "the peer has no source-folder binding" test -e "$T/peer/store/projects/native/meta/workspace.json"
ok "sync preserves source-folder routing" l status --porcelain
ok "original project passes verification" l fsck
finish
