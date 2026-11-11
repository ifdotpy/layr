#!/bin/bash
# ci: macos
# State records name their parts (docs/overlays.md section 1a, slice 4): a named save records the
# part of each follow overlay in the new state, `line new` and a hard reset take the parts of their
# state, a merge combines the parts glass by glass, `view <state>` shows them, the overlay store gets
# no overlay line for them, and `overlay migrate-parts` gives parts to a head saved before them.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p
cd "$P/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true

# parts <state>: the parts of a state record as `glass=part` pairs, each part as the short id that
# `layr overlay use` prints.
parts() {
    python3 - "$P/meta/db/layr.sqlite" "$1" <<'PY'
import json, sqlite3, sys, uuid
row = sqlite3.connect(sys.argv[1]).execute("SELECT rec FROM states WHERE id=? OR display=?", (sys.argv[2], sys.argv[2])).fetchone()
def short(u):
    b = uuid.UUID(u).bytes
    return (b[10:16] + b[6:10] + b[0:6]).hex()[:7]
print(" ".join(f"{k}={short(v)}" for k, v in sorted(json.loads(row[0]).get("parts", {}).items())) if row else "missing")
PY
}
# overlay_lines <overlay>: the number of lines of the overlay store.
overlay_lines() {
    python3 - "$P/meta/overlays/$1/meta/db/layr.sqlite" <<'PY'
import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("SELECT COUNT(*) FROM lines WHERE deleted=0").fetchone()[0])
PY
}
head_of() { l rev-parse "$1"; }
part_of() { l overlay use | awk -v o="$1" '$1==o{print $2}'; }

# 1. A save of a shared change with a follow overlay records the part in the new state.
ok "a follow overlay" l overlay create notes --mode follow -q
echo n1 > notes.md
l overlay move notes notes.md >/dev/null
echo s1 > s1.txt
l add s1.txt >/dev/null
ok "a save of the shared set and the overlay" l save -qm "notes 1"
h1=$(head_of head)
p1=$(part_of notes)
eq "the state names the part that the line shows" "notes=$p1" "$(parts "$h1")"
no "the shared state has no overlay file" l show head:notes.md
eq "the overlay store has no overlay line for main" 0 "$(overlay_lines notes)"
eq "the overlay reads its file at the line's part" n1 "$(l overlay show notes head:notes.md)"
eq "and lists its history" "notes 1|overlay notes" "$(l overlay log notes --format=%s | paste -sd'|' -)"

echo n2 > notes.md
echo s2 > s2.txt
l add s2.txt >/dev/null
ok "a second save" l save -qam "notes 2"
h2=$(head_of head)
p2=$(part_of notes)
no "the line shows a new part" test "$p2" = "$p1"
eq "the new state names it" "notes=$p2" "$(parts "$h2")"
eq "the first state keeps its part" "notes=$p1" "$(parts "$h1")"
eq "still no overlay line" 0 "$(overlay_lines notes)"

# 2. line new takes the parts of its start state.
ok "a line from main" l line new -q a main
eq "it has the newest notes" n2 "$(cat "$P/lines/a/notes.md")"
ok "a line from the first state" l line new -q old "$h1"
eq "it has the notes of that state" n1 "$(cat "$P/lines/old/notes.md")"
eq "and is clean" "" "$(cd "$P/lines/old" && l status --porcelain)"
eq "line new made no overlay lines" 0 "$(overlay_lines notes)"

# 3. view and reset read the parts of a state.
eq "the view of the first state names its part" "notes" "$(l view "$h1" --json | python3 -c 'import json,sys; print(" ".join(x["overlay"] for x in json.load(sys.stdin)["view"]["layers"]))')"
ok "a hard reset to the first state" l reset -q --hard "$h1"
eq "the folder has the notes of that state" n1 "$(cat notes.md)"
eq "the line is clean" "" "$(l status --porcelain)"
ok "and back to the second" l reset -q --hard "$h2"
eq "the newer notes come back" n2 "$(cat notes.md)"

# 4. A merge combines the parts: a three-way merge of the overlay with the shared set.
cd "$P/lines/a" || exit 1
printf 'top\nn2\n' > notes.md
echo a > a.txt
l add a.txt >/dev/null
ok "a changes the notes and the shared set" l save -qam "a side"
cd "$P/lines/main" || exit 1
printf 'n2\nbottom\n' > notes.md
echo m > m.txt
l add m.txt >/dev/null
ok "main changes them too" l save -qam "main side"
out=$(l merge a 2>&1); rc=$?
eq "the merge works" 0 "$rc"
has "it names the overlay" "Merged overlays: notes" "$out"
eq "both overlay changes are in" "top|n2|bottom" "$(paste -sd'|' notes.md)"
ok "and the shared file" test -e a.txt
eq "main is clean" "" "$(l status --porcelain)"
hm=$(head_of head)
no "the merge made a new part" test "$(part_of notes)" = "$p2"
eq "the merge state names the merged part" "notes=$(part_of notes)" "$(parts "$hm")"
eq "the merged part reads back" "top|n2|bottom" "$(l overlay show notes head:notes.md | paste -sd'|' -)"
eq "the merge made no overlay line" 0 "$(overlay_lines notes)"

# 5. fetch --line takes the parts that the states of the line name, and no part of another line.
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
read id key <<<"$(env LAYR_ROOT="$T/y/store" "$LAYR" remote self --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$id" --public-key "$key" >/dev/null
l overlay readers notes "$(id -un)" "machine:$id" >/dev/null
l push cloud -q >/dev/null 2>&1
yl() { env LAYR_ROOT="$T/y/store" "$LAYR" "$@"; }
ok "a clone of main" yl clone "file://$T/archive" p --heads --only-line main -q
echo n3 >> notes.md
echo s3 > s3.txt
l add s3.txt >/dev/null
l save -qam "notes 3"
main_part=$(part_of notes)
cd "$P/lines/a" || exit 1
echo other >> notes.md
echo o > o.txt
l add o.txt >/dev/null
l save -qam "a again"
a_part=$(part_of notes)
l push cloud -q >/dev/null 2>&1
# has_part <short id>: the overlay store of the clone has that part.
has_part() {
    ls "$T/y/store/projects/p/meta/overlays/notes/states" 2>/dev/null | python3 -c '
import sys, uuid
for n in sys.stdin.read().split():
    b = uuid.UUID(n).bytes
    if (b[10:16] + b[6:10] + b[0:6]).hex().startswith(sys.argv[1]):
        sys.exit(0)
sys.exit(1)' "$1"
}
cd "$T/y/store/projects/p/lines/main" || exit 1
ok "a fetch of main on the base and notes" yl fetch origin --line main --overlay base,notes -q
ok "it takes the part of the new state of main" has_part "$main_part"
no "it leaves the part of line a on the remote" has_part "$a_part"
cd "$P/lines/main" || exit 1

# 6. A store saved before parts: on a protected line (which moves only through merges) an
#    overlay-only save still leaves the head without parts, as every save did before parts.
l init q --from "$T/src" -q >/dev/null
Q=$LAYR_ROOT/projects/q
cd "$Q/lines/main" || exit 1
l protect main >/dev/null 2>&1 || true
l overlay create keys --mode follow -q
echo k1 > keys.txt
l overlay move keys keys.txt >/dev/null
ok "an overlay-only save" l save -qm "keys 1"
qh=$(head_of head)
P=$Q
eq "the head names no part" "" "$(parts "$qh")"
out=$(l overlay migrate-parts -n)
has "a dry run names the head" "would give parts to 1 state(s) of 1 line(s)" "$out"
eq "and writes nothing" "" "$(parts "$qh")"
ok "the migration" l overlay migrate-parts -q
eq "the head names the part of the line" "keys=$(part_of keys)" "$(parts "$qh")"
has "a second run has nothing to do" "no state to migrate" "$(l overlay migrate-parts)"
echo k2 > keys.txt
ok "a later overlay-only save" l save -qam "keys 2"
ok "a line from the migrated state" l line new -q at "$qh"
eq "it has the part that the migration named" k1 "$(cat "$Q/lines/at/keys.txt")"
ok "a line from main" l line new -q now main
eq "it has the part that main shows now" k2 "$(cat "$Q/lines/now/keys.txt")"

# 7. A line has its own history: on a line that is not protected an overlay-only save makes a
#    state of the line with the base tree of its parent and the new part. Git gets no empty commit.
cd "$Q/lines/now" || exit 1
before=$(head_of head)
echo k3 > keys.txt
ok "an overlay-only save on a free line" l save -qam "keys 3"
after=$(head_of head)
no "it moves the head" test "$before" = "$after"
eq "the new head names the new part" "keys=$(part_of keys)" "$(parts "$after")"
git init -q --bare "$T/q.git"
ok "a git remote" l git remote add gq "$T/q.git"
ok "a git push of the line" l git push -q gq now
git_of() { l log -n 1 "$1" 2>/dev/null | sed -n 's/^Git: *//p'; }
no "the parent has a commit" test -z "$(git_of "$before")"
eq "the new state has the commit of its parent" "$(git_of "$before")" "$(git_of "$after")"

finish
