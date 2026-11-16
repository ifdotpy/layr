#!/bin/bash
# ci: macos overlayfs
# Protected merges reject conflicts before records, automatic saves, overlay lines or folder writes.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init p --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/p
M=$P/lines/main
cd "$M"
l protect main --remove >/dev/null
l line new -q topic main || exit 1
cd ../topic
printf 'source\n' > notes.txt
l save -qam source || exit 1
source=$(l rev-parse head)
cd "$M"
printf 'target\n' > notes.txt
l save -qam target || exit 1
head=$(l rev-parse head)
l protect main >/dev/null || exit 1

# Snapshot recorded history, line heads, state trees, staging and every working file. Ignore
# derived status caches and read-only assembled views; they do not change the proposed result.
snapshot() {
    python3 - "$P" <<'PY'
import hashlib, json, os, pathlib, sqlite3, stat, sys
p = pathlib.Path(sys.argv[1])
result = {}
for dbpath in sorted(p.glob('**/meta/db/layr.sqlite')):
    db = sqlite3.connect(dbpath)
    result[str(dbpath.relative_to(p))] = {
        table: sorted(db.execute('SELECT ' + column + ' FROM ' + table).fetchall())
        for table, column in [('records', 'raw'), ('states', 'rec'), ('lines', '*'), ('bases', '*')]
    }
    known = {row[0] for row in db.execute('SELECT id FROM states')}
    db.close()
    project = dbpath.parent.parent.parent
    for name in ['states', 'deltas', 'meta/stage', 'meta/scan', 'meta/work', 'meta/pending']:
        root = project / name
        entries = []
        if root.exists():
            for path in sorted(root.rglob('*')):
                # APFS can materialize a view of a recorded thin state while reading it.
                if name == 'states' and path.relative_to(root).parts[0] in known:
                    continue
                m = path.lstat()
                data = os.readlink(path) if path.is_symlink() else (
                    hashlib.sha256(path.read_bytes()).hexdigest() if stat.S_ISREG(m.st_mode) else '')
                entries.append((str(path.relative_to(root)), stat.S_IFMT(m.st_mode), stat.S_IMODE(m.st_mode), data))
        result[str(root.relative_to(p))] = entries
root = p / 'lines/main'
result['working'] = []
for path in sorted(root.rglob('*')):
    m = path.lstat()
    data = os.readlink(path) if path.is_symlink() else (
        hashlib.sha256(path.read_bytes()).hexdigest() if stat.S_ISREG(m.st_mode) else '')
    result['working'].append((str(path.relative_to(root)), stat.S_IFMT(m.st_mode), stat.S_IMODE(m.st_mode), data))
registry = p.parent.parent / '.layr-overlayfs/registry.json'
if registry.exists():
    result['overlayfs'] = [json.loads(registry.read_text()), sorted(x.name for x in (registry.parent / 'objects').iterdir())]
print(json.dumps(result, sort_keys=True))
PY
}
refuses() {
    local name=$1 source=$2 state=$3
    shift 3
    local before out rc
    before=$(snapshot) || exit 1
    out=$(l merge "$source" --expect "$state" "$@" 2>&1); rc=$?
    eq "$name refuses" 1 "$rc"
    has "$name explains protection" 'protected line' "$out"
    has "$name explains how to resolve" 'merge the target into the source line, resolve there, and merge again' "$out"
    eq "$name leaves records, heads, files, staging and state trees unchanged" "$before" "$(snapshot)"
}

refuses 'three-way merge without checks' topic "$source"
refuses 'squash merge without checks' topic "$source" --squash
# Dirty target content would formerly have produced an automatic save before the refusal.
printf 'local dirty work\n' >> src/main.rs
printf 'untracked\n' > local.txt
mkdir -p target || exit 1
printf 'dirty cached work\n' > target/out.bin
refuses 'merge with dirty target content' topic "$source"
eq 'dirty target content survives' 'local dirty work' "$(tail -1 src/main.rs)"
eq 'dirty cached content survives' 'dirty cached work' "$(cat target/out.bin)"
l restore src/main.rs
rm local.txt

# Recovery stays in the source line. The unprotected merge still records its conflicts.
cd ../topic
no 'merge main into the source reports conflicts' l merge main
printf 'resolved\n' > notes.txt
l add notes.txt && l save -qm resolved || exit 1
resolved=$(l rev-parse head)
cd "$M"
ok 'the reviewed resolution merges into protected main' l merge topic --expect "$resolved" -q
eq 'main contains the resolution' resolved "$(cat notes.txt)"
eq 'the resolution becomes the protected head' "$resolved" "$(l rev-parse head)"

# A fast-forward must inspect the existing source state's conflicts, not just a fresh outcome.
l line new -q left main && l line new -q right main || exit 1
cd ../left
printf 'left\n' > notes.txt
l save -qam left || exit 1
cd ../right
printf 'right\n' > notes.txt
l save -qam right || exit 1
no 'an ordinary source line can have conflicts' l merge left
conflicted=$(l rev-parse head)
cd "$M"
refuses 'fast-forward to a conflicted source' right "$conflicted"
refuses 'no-ff merge from a conflicted source' right "$conflicted" --no-ff
refuses 'squash merge from a conflicted source' right "$conflicted" --squash
l protect main --remove >/dev/null
printf 'unrelated target change\n' > unrelated.txt
l add unrelated.txt && l save -qm unrelated || exit 1
l protect main >/dev/null
refuses 'divergent merge from a conflicted source' right "$conflicted"

# A clean three-way merge remains allowed without checks.
l line new -q clean-a main && l line new -q clean-b main || exit 1
cd ../clean-a
printf 'a\n' > a.txt
l add a.txt && l save -qm a || exit 1
a=$(l rev-parse head)
cd ../clean-b
printf 'b\n' > b.txt
l add b.txt && l save -qm b || exit 1
b=$(l rev-parse head)
cd "$M"
ok 'clean fast-forward into protected main' l merge clean-a --expect "$a" -q
ok 'clean three-way into protected main' l merge clean-b --expect "$b" -q
ok 'both clean changes are present' test -f a.txt -a -f b.txt

# Overlay-only conflicts exercise the path where the shared head is already up to date.
l protect main --remove >/dev/null
l overlay create notes -q || exit 1
printf 'base overlay\n' > overlay.txt
l overlay move notes overlay.txt >/dev/null
l save -qam overlay-base || exit 1
l line new -q overlay-topic main || exit 1
cd ../overlay-topic
# A protected source can save its overlays without moving its shared head.
l protect overlay-topic >/dev/null
printf 'source overlay\n' > overlay.txt
l save -qam overlay-source || exit 1
part_source=$(l rev-parse head)
cd "$M"
printf 'target overlay\n' > overlay.txt
l save -qam overlay-target || exit 1
l protect main >/dev/null
printf 'dirty overlay target\n' >> src/main.rs
refuses 'overlay conflict without checks' overlay-topic "$part_source"

# An existing overlay conflict must not disappear when the target changes another overlay path.
l restore src/main.rs
l line new -q overlay-left main && l line new -q overlay-right main || exit 1
cd ../overlay-left
printf 'left overlay\n' > overlay.txt
l save -qam overlay-left || exit 1
cd ../overlay-right
printf 'right overlay\n' > overlay.txt
l save -qam overlay-right || exit 1
no 'an ordinary source records overlay conflicts' l merge overlay-left
has 'the source has an unresolved overlay path' 'UU overlay.txt' "$(l status -s)"
conflicted_overlay=$(l rev-parse head)
cd "$M"
l protect main --remove >/dev/null
printf 'unrelated overlay target change\n' > unrelated-overlay.txt
l overlay move notes unrelated-overlay.txt >/dev/null
l save -qam unrelated-overlay || exit 1
l protect main >/dev/null
refuses 'divergent overlay merge from a conflicted source' overlay-right "$conflicted_overlay"

# A missing overlay line must remain missing even when planning wants to create it. Use a
# separate incoming overlay and a shared-set conflict, so planning precedes the refusal.
l restore src/main.rs
l line new -q missing-overlay main || exit 1
cd ../missing-overlay
# The tree of overlays (docs/overlays.md section 1): `incoming` is above the overlay that the folder shows, so the
# move makes it the view. A save writes the overlay of the view, so the change of main is a save with --to main.
l overlay create incoming --parent notes -q || exit 1
printf 'incoming\n' > incoming.txt
l overlay move incoming incoming.txt >/dev/null
l save -qm incoming-part || exit 1
printf 'incoming shared\n' > notes.txt
l add notes.txt && l save -qm incoming --to main || exit 1
incoming=$(l rev-parse head)
cd "$M"
l protect main --remove >/dev/null
printf 'target shared\n' > notes.txt
l save -qam target-shared --to main || exit 1
# Keep a real staging tree, with no staged delta, across the conflict refusal.
l add notes.txt || exit 1
l protect main >/dev/null
refuses 'conflict after planning a missing overlay line' missing-overlay "$incoming"
cd ../missing-overlay
no 'resolve the missing-overlay change in its source' l merge main
printf 'resolved incoming\n' > notes.txt
l add notes.txt && l save -qm incoming-resolved --to main || exit 1
incoming=$(l rev-parse head)
cd "$M"
ok 'a clean merge creates the deferred overlay line' l merge missing-overlay --expect "$incoming" -q
# The view of a line does not change in a merge (owner decision 2026-11-15): the line keeps the merged part, and
# its folder shows it after `overlay use`.
no 'the folder of main does not show the new overlay' test -e incoming.txt
ok 'overlay use shows the merged part' l overlay use incoming -q
eq 'the new overlay is visible after overlay use' incoming "$(cat incoming.txt)"
eq 'the shared resolution is visible too' 'resolved incoming' "$(cat notes.txt)"

finish
