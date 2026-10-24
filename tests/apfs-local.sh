#!/bin/bash
# Local APFS commands, with the default main protection and the real caller identity.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
printf 'read only\n' > "$T/src/readonly.txt"
chmod 444 "$T/src/readonly.txt"
l init local --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/local/lines
cd "$P/main" || exit 1
eq "import preserves a read-only file" "read only" "$(cat readonly.txt)"
no "main remains protected" l save --allow-empty -qm forbidden
l line new -q topic
cd "$P/topic" || exit 1
printf 'node_modules/\n' >> .gitignore
printf 'node_modules regenerable\n' > .layrattributes
l add -A && l save -qm attributes
mkdir -p node_modules/pkg
printf 'cache\n' > node_modules/pkg/data
ok "save converts a regenerable folder" l save -q --caches
has "the folder is a layer" "node_modules" "$(l cache list)"
no "layer data is absent from history" l show head:node_modules/pkg/data
l line new -q warm 'head@{0}'
eq "new line receives the saved layer" "cache" "$(cat "$P/warm/node_modules/pkg/data")"
printf 'private\n' > "$P/warm/node_modules/pkg/data"
eq "the source layer is separate" "cache" "$(cat node_modules/pkg/data)"
printf 'topic\n' > result.txt
l add result.txt && l save -qm result
head=$(l rev-parse head)
cd "$P/main" || exit 1
no "protected merge requires the expected state" l merge topic
ok "protected merge accepts the expected state" l merge topic --expect "$head"
eq "main receives the file" "topic" "$(cat result.txt)"
ok "state signatures and snapshots pass fsck" l fsck
l line delete -D -q warm
ok "gc can remove obsolete APFS trees" l gc -q
ok "states remain valid after gc" l fsck
cd "$P/topic" || exit 1
printf 'private value\n' > private.env
ok "create an APFS overlay" l overlay create local -q
ok "move a file to the overlay" l overlay move local private.env
ok "commit an APFS overlay" l save -qm private
eq "overlay keeps the file" "private value" "$(l overlay show local head:private.env)"
no "shared history excludes the overlay" l show head:private.env
eq "overlay status is clean" "" "$(l status --porcelain)"
printf 'new value\n' > private.env
ok "commit an overlay change" l save -qam update
eq "overlay history has the change" "new value" "$(l overlay show local head:private.env)"
ok "disable the overlay" l overlay use --none
no "disabled overlay leaves the worktree" test -e private.env
ok "restore the overlay" l overlay use local
eq "restored overlay has the content" "new value" "$(cat private.env)"
ok "overlay snapshots pass fsck" l fsck
finish
