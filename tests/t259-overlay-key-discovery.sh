#!/bin/bash
# ci: macos overlayfs
# Public and hidden manifests are discovered without traversing cache data.
. "$(dirname "$0")/lib.sh"
set -e

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l protect main --remove >/dev/null 2>&1 || true
l overlay create notes -q
echo visible > visible.txt
l overlay move notes visible.txt -q
l save -m visible -q
machine=$(l remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
l overlay create secrets --hidden --readers "machine:$machine" -q
l overlay use secrets -q
echo hidden > hidden.txt
l overlay move secrets hidden.txt -q
l save -m hidden -q
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q

# An excluded known overlay must not have its manifest read, even while fetching another overlay.
hidden_key=$(find "$T/archive/overlays" -path '*/keys/*.age' | head -1)
cp "$hidden_key" "$T/hidden-key.backup"
printf 'invalid encrypted manifest\n' > "$hidden_key"
ok "selected visible fetch skips the known hidden manifest" l fetch archive --overlay notes -q
# Main-only skips all overlay discovery, including malformed visible manifests.
visible_key=$(find "$T/archive/overlays" -path '*/keys/*.json' | head -1)
cp "$visible_key" "$T/visible-key.backup"
printf 'invalid visible manifest\n' > "$visible_key"
report=$(l fetch archive --overlay main --json)
ok "main-only fetch reports one selection summary" python3 -c '
import json,sys
assert json.loads(sys.argv[1])["omitted"] == ["overlays outside --overlay were skipped"]
' "$report"
cp "$T/hidden-key.backup" "$hidden_key"
cp "$T/visible-key.backup" "$visible_key"

# Any recursive overlays/ listing would encounter these forbidden entries.
# The epoch's own records and slots remain readable for hidden discovery.
for epoch in "$T"/archive/overlays/*/epochs/*; do
    mkdir -p "$epoch/chunks"
    ln -s missing "$epoch/chunks/discovery-must-not-read"
done
original_root=$LAYR_ROOT
new_fs "$T/replica" 2G
export LAYR_ROOT=$T/replica LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
ok "base-only clone skips overlay discovery" l clone "file://$T/archive" p --heads --overlay main -q
cd "$LAYR_ROOT/projects/p/lines/main"
ok "base-only fetch does not list cache data" l fetch origin --heads --overlay main -q
# A base-only clone has not imported the hidden overlay's metadata yet. Discover
# the remote overlays before selecting their names, which are validated locally.
report=$(l fetch origin --overlay notes --json)
ok "selected visible overlay without a local identity is discovered" python3 -c '
import json,sys
assert any(row.startswith("notes:") for row in json.loads(sys.argv[1])["overlays"])
' "$report"
report=$(l fetch origin --json)
ok "public and hidden overlays share discovery without cache listings" python3 -c '
import json,sys
report=json.loads(sys.argv[1])
for name in ["secrets"]:
    assert any(row.startswith(name + ":") for row in report["overlays"]), report
    assert not any(row.startswith("overlay " + name + ":") for row in report["omitted"]), report
' "$report"
ok "public and hidden overlays fetch through the same discovered key list" l fetch origin --overlay notes,secrets -q
# Fetch restores snapshots; read them through the caller's overlay access policy.
# Updating an existing line's working view is separate from discovery.
eq "visible overlay content survives discovery" visible "$(l overlay show notes head:visible.txt)"
eq "hidden overlay content survives slot discovery" hidden "$(l overlay show secrets head:hidden.txt)"
export LAYR_ROOT=$original_root
finish
