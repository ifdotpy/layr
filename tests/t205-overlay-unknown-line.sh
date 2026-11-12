#!/bin/bash
# ci: macos overlayfs
# An absent parent line row is not proof that the line was deleted.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main"
mkdir -p target
echo parent > target/out
l save --overlays -q
l line new missing main >/dev/null
cd "$P/lines/missing"
echo child > target/out
l save --overlays -q
cd "$P/lines/main"
db=$P/meta/db/layr.sqlite
odb=$P/meta/overlays/$TG/meta/db/layr.sqlite
line=$(python3 - "$db" "$T/parent-row.json" <<'PYROW'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.row_factory = sqlite3.Row
row = c.execute("SELECT * FROM lines WHERE name='missing' AND deleted=0").fetchone()
assert row
with open(sys.argv[2], 'w') as f: json.dump(dict(row), f)
c.execute('DELETE FROM lines WHERE id=?', (row['id'],)); c.commit()
print(row['id'])
PYROW
) || exit 1
count_lines() {
    python3 - "$odb" "$line" <<'PYLINES'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(c.execute('SELECT COUNT(*) FROM lines WHERE deleted=0 AND name LIKE ?', ('line-' + sys.argv[2] + '%',)).fetchone()[0])
PYLINES
}
before=$(count_lines)
[ "$before" -gt 0 ] || { echo 'FATAL: overlay fixture has no child line'; exit 1; }
out=$(l fsck --repair 2>&1); rc=$?
eq 'fsck repair completes' 0 "$rc"
has 'fsck checks the cache overlay' "overlay $TG:" "$out"
eq 'fsck repair does not delete an overlay line whose parent row is missing' "$before" "$(count_lines)"
# Restore the deliberately removed derived row before the normal garbage check.
python3 - "$db" "$T/parent-row.json" <<'PYROW'
import json, sqlite3, sys
with open(sys.argv[2]) as f: row = json.load(f)
c = sqlite3.connect(sys.argv[1]); cols = list(row)
c.execute('INSERT OR REPLACE INTO lines (' + ','.join(cols) + ') VALUES (' + ','.join('?' for _ in cols) + ')', [row[k] for k in cols]); c.commit()
PYROW
finish
