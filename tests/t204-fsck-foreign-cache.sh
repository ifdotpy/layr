#!/bin/bash
# ci: macos overlayfs
# Remote cache records from other platforms do not promise a local tree. The fixture is a
# `cache.target` glass of an older layr: one glass for every platform, with an overlay line per
# platform. A new glass has one platform (docs/overlays.md section 1a, slice 3; t138).
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
l overlay create cache.target --folder target --quiet --history last --merge union-theirs -q
l config overlay.cache.target.cache true >/dev/null
mkdir -p target
echo local > target/out.bin
l save --glasses -q
echo foreign > target/out.bin
LAYR_TEST_PLATFORM=plan9-mips l save --glasses -q
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
l push archive -q >/dev/null
new_fs "$T/peer" 2G
export LAYR_ROOT=$T/peer/store
LAYR_PAPER_KEY_FILE=$T/paper.txt l clone "file://$T/archive" p >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
O=$LAYR_ROOT/projects/p/meta/overlays/cache.target
odb=$O/meta/db/layr.sqlite
foreign=$(python3 - "$odb" <<'PYCACHE'
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
rows = [(s, p) for s, p, raw in con.execute('SELECT id,present,rec FROM states')
        if json.loads(raw).get('layer_platform', '').startswith('plan9-mips')]
assert rows and all(p == 0 for _, p in rows), rows
print(rows[-1][0])
PYCACHE
) || exit 1
[ -n "$foreign" ] || exit 1
ok "the remote-platform fixture has no local tree" test ! -d "$O/states/$foreign"
out=$(l fsck 2>&1); rc=$?
eq "fsck accepts intentionally absent foreign cache data" 0 "$rc"
has "fsck explains the foreign cache head" 'another cache platform' "$out"
# A present flag promises storage even for a foreign platform. It must still be diagnosed.
python3 - "$odb" "$foreign" <<'PYCACHE'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('UPDATE states SET present=1 WHERE id=?', (sys.argv[2],)); con.commit()
PYCACHE
out=$(l fsck 2>&1); rc=$?
eq "fsck rejects a falsely present foreign cache" 1 "$rc"
has "the missing promised cache head is named" 'is missing' "$out"
python3 - "$odb" "$foreign" <<'PYCACHE'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('UPDATE states SET present=0, deleted=1 WHERE id=?', (sys.argv[2],)); con.commit()
PYCACHE
# An absent foreign head is not acceptable when its state is marked deleted.
out=$(l fsck 2>&1); rc=$?
eq "fsck rejects a deleted foreign cache head" 1 "$rc"
has "the deleted foreign head is named" 'is missing' "$out"
python3 - "$odb" "$foreign" <<'PYCACHE'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('UPDATE states SET deleted=0 WHERE id=?', (sys.argv[2],)); con.commit()
PYCACHE
# The same absence on this platform remains a fault, even with present=0.
local_head=$(python3 - "$odb" <<'PYCACHE'
import json, sqlite3, sys
con = sqlite3.connect(sys.argv[1])
rows = [(s, json.loads(raw)) for s, raw in con.execute(
    'SELECT s.id,s.rec FROM states s JOIN lines l ON l.head=s.id WHERE s.present=1')]
print(next(s for s, r in rows if r.get('layer_platform') and not r['layer_platform'].startswith('plan9-mips')))
PYCACHE
) || exit 1
[ -n "$local_head" ] || exit 1
if [ "$(uname -s)" = Darwin ]; then chflags nouchg "$O/states/$local_head"; fi
mv "$O/states/$local_head" "$T/local-cache"
python3 - "$odb" "$local_head" <<'PYCACHE'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('UPDATE states SET present=0 WHERE id=?', (sys.argv[2],)); con.commit()
PYCACHE
out=$(l fsck 2>&1); rc=$?
eq "fsck still rejects a required local cache without a tree" 1 "$rc"
has "fsck names the required local head" 'is missing' "$out"
mv "$T/local-cache" "$O/states/$local_head"
if [ "$(uname -s)" = Darwin ]; then chflags uchg "$O/states/$local_head"; fi
python3 - "$odb" "$local_head" <<'PYCACHE'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute('UPDATE states SET present=1 WHERE id=?', (sys.argv[2],)); con.commit()
PYCACHE
finish
