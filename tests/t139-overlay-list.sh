#!/bin/bash
# ci: macos overlayfs
# Listing includes public, hidden and cache overlays, but not removed cache overlays.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
printf 'target regenerable\n' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/p/lines/main"
l protect main --remove >/dev/null 2>&1 || true
l overlay create public --mode follow --quiet --history last -q || exit 1
l overlay create private --mode follow --hidden --history 3 -q || exit 1
l overlay use --add public private >/dev/null || exit 1
mkdir -p target
printf cache > target/result
l save --caches -q || exit 1
# A second cache is removed while its overlay project still exists.
printf 'other regenerable\n' >> .layrattributes
printf 'other/\n' >> .gitignore
mkdir -p other
printf removed > other/result
l save --caches -q || exit 1
l overlay remove cache.other >/dev/null || exit 1
# A replica can know a public overlay descriptor before receiving its project.
python3 - "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" <<'PYFIXTURE'
import json, os, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    value = {'hidden': False, 'owner': os.getuid(), 'mode': 'follow', 'root': ''}
    db.execute('INSERT INTO config(key, value, ord) VALUES (?, ?, ?)',
               ('overlay.offline', json.dumps(value), 'fixture'))
PYFIXTURE
l overlay list --json > "$T/list.json" || exit 1
ok "overlay list JSON matches the golden fields" python3 - "$T/list.json" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    rows = json.load(f)
assert [r['name'] for r in rows] == ['cache.target', 'offline', 'private', 'public'], rows
by_name = {r['name']: r for r in rows}
for row in rows:
    assert set(row) == {'name', 'mode', 'hidden', 'owner', 'readers', 'shown', 'properties', 'folder', 'state', 'size', 'saved'}, row
    assert isinstance(row['owner'], int) and isinstance(row['readers'], list), row
    assert row['mode'] == 'follow', row
assert by_name['offline']['readers'] == [] and by_name['offline']['saved'] is None, by_name['offline']
assert by_name['private']['hidden'] and by_name['private']['shown'], by_name['private']
assert by_name['private']['properties'] == {'history': 3, 'quiet': False, 'merge': 'three-way', 'gc': 'none', 'prune': 'auto'}
assert not by_name['public']['hidden'] and by_name['public']['shown'], by_name['public']
assert by_name['public']['properties'] == {'history': 1, 'quiet': True, 'merge': 'three-way', 'gc': 'none', 'prune': 'auto'}
cache = by_name['cache.target']
assert cache['folder'] == 'target' and cache['state'] == 'cache', cache
assert cache['properties'] == {'history': 1, 'quiet': True, 'merge': 'union-theirs', 'gc': 'none', 'prune': 'auto'}, cache
assert isinstance(cache['size'], int) and isinstance(cache['saved'], str), cache
PY
out=$(l overlay list)
has "text listing shows public properties" 'history=last quiet' "$out"
has "text listing shows the cache folder" 'folder=target' "$out"
eq "removed cache is absent from text listing" 0 "$(printf '%s\n' "$out" | grep -c cache.other)"
finish
