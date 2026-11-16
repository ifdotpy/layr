#!/bin/bash
# ci: macos overlayfs
# Issue #443: when native copies cannot be retired (here an overlay whose database is missing), `layr gc` warns and
# still prunes and repacks the object store. The free step after each command keeps the copies for a later retry.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
O=$P/meta/overlays/$TG
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l migrate >/dev/null 2>&1
eq "the project keeps its states as objects" "true" "$(l config objects.enabled 2>/dev/null | tr -d '"' | tail -1)"
mkdir -p target/debug
echo build > target/debug/out.bin
echo a >> notes.txt
ok "a named save with build output" l save -qam "first build"
ok "the cache overlay has a database" test -f "$O/meta/db/layr.sqlite"
ok "the native copy queue exists" test -d "$P/meta/object-native"

# The overlay cannot open: its database is gone for a while.
mv "$O/meta/db/layr.sqlite" "$T/overlay.sqlite"
out=$(l gc 2>&1)
rc=$?
eq "gc succeeds when native copies cannot be retired" 0 "$rc"
has "gc warns that native copies remain" "warning: native copies not retired" "$out"
has "and still repacks the object store" "objects: " "$out"
mv "$T/overlay.sqlite" "$O/meta/db/layr.sqlite"
ok "gc passes again when the overlay opens" l gc -q
finish
