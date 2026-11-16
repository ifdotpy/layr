#!/bin/bash
# ci: macos macos-plain
# A virtual view's ignored build output must not survive publication at another execution location.
if [ "$(uname -s)" != Darwin ]; then
    echo "t262-view-cache-location.sh (needs macOS): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"
export LAYR_EXPERIMENTAL_VIEWS=1
sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init viewloc --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/viewloc
cd "$P/lines/main"
l protect main --remove >/dev/null 2>&1 || true
# The hidden view fixture uses the real view metadata/upper format without requiring an FSKit mount.
l line new -q virtual main >/dev/null
id=$(l line list --json | python3 -c 'import json,sys; print(next(x["id"] for x in json.load(sys.stdin) if x["name"]=="virtual"))')
head=$(l log --json -n 1 | python3 -c 'import json,sys; print(json.loads(next(sys.stdin))["id"])')
view=$P/meta/vlines/$id
mv "$P/lines/virtual" "$T/original-native"
mkdir -p "$view/upper/target"
cp "$T/original-native/target/.layr-cache-location" "$view/upper/target/.layr-cache-location"
echo foreign > "$view/upper/target/output"
python3 - "$view/view.json" "$id" "$head" "$P/states/$head" <<'PY'
import json, sys
json.dump({'line':sys.argv[2], 'name':'virtual', 'base':sys.argv[3], 'lower':sys.argv[4], 'mounted':False}, open(sys.argv[1], 'w'))
PY
cd "$T"
ok "a hidden view publishes as a native folder" l --project viewloc line show virtual --folder "$T/native"
no "foreign ignored build output is absent at the new location" test -e "$T/native/target/output"
ok "the destination has newly established provenance" test -f "$T/native/target/.layr-cache-location"
python3 - "$T/native" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1]).resolve()
p = json.loads((root / 'target/.layr-cache-location').read_text())['location']
assert p == {'workspace':str(root), 'target':str(root / 'target')}, p
PY
eq "the marker names the final destination" 0 "$?"
finish
