#!/bin/bash
# Overlay kinds and the old properties (docs/cache-overlays.md): history, quiet, merge union-theirs. One user,
# so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf '.env\n' >> $T/src/.gitignore
l init p --from $T/src -q >/dev/null
M=$LAYR_ROOT/projects/p/lines/main
O=$LAYR_ROOT/projects/p/meta/overlays
cd $M
l protect main --remove >/dev/null 2>&1 || true
states() { ls "$O/$1/states" | wc -l | tr -d ' '; }

# 1. Kinds instead of properties (docs/overlays.md section 1, owner decision 2026-11-15): the options that a
#    kind fixes fail with exit status 2 and name the kind rule; a cache option fails on a patches overlay.
ok "a patches overlay" l overlay create notes -q
for opt in "--quiet" "--no-quiet" "--merge union-theirs" "--trust readers" "--mode follow"; do
    # shellcheck disable=SC2086
    eq "overlay create $opt fails with exit 2" 2 "$(l overlay create x $opt >/dev/null 2>&1; echo $?)"
done
for opt in "--quiet" "--merge union-theirs" "--trust readers" "--history last" "--gc lru:20G" "--prune none"; do
    # shellcheck disable=SC2086
    eq "overlay set notes $opt fails with exit 2" 2 "$(l overlay set notes $opt >/dev/null 2>&1; echo $?)"
done
has "the refusal names the kind rule" "an overlay has a kind" "$(l overlay set notes --history last 2>&1)"
has "list shows the kind" "kind=patches" "$(l overlay list)"
eq "show --json has the kind" patches "$(l overlay show notes --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["kind"])')"
# Old data: settings that an older layr stored on a patches overlay still act (docs/overlays.md, kinds).
l config overlay.notes.history 1 >/dev/null
l config overlay.notes.merge '"union-theirs"' >/dev/null
has "list shows the stored old settings" "history=last merge=union-theirs" "$(l overlay list)"
props=$(l overlay show notes --json | python3 -c 'import json,sys; p=json.load(sys.stdin)["properties"]; print(p["history"], p["quiet"], p["merge"], "gc" in p)')
eq "show --json has them, and no gc" "1 False union-theirs False" "$props"
eq "an overlay without settings shows its kind only" "kind=patches" "$(l overlay create plain -q && l overlay list | grep '^  plain' | cut -f5)"

# 2. history=last keeps the newest state of each overlay line and the empty first state.
echo n1 > notes.md
l overlay move notes notes.md >/dev/null
ok "first overlay state" l save -m n1 -q
echo n2 > notes.md
ok "second overlay state" l save -a -m n2 -q
echo n3 > notes.md
ok "third overlay state" l save -a -m n3 -q
eq "only the first state and the head stay" 2 "$(states notes)"
eq "the history records stay" 4 "$(l overlay log notes --oneline | wc -l | tr -d ' ')"
eq "the head has the newest file" n3 "$(l overlay show notes head:notes.md)"

# 3. merge=union-theirs: the target gets every file of both sides; the incoming side wins.
l line new topic main >/dev/null
cd $LAYR_ROOT/projects/p/lines/topic
echo topic > notes.md
echo b > b.md
l overlay move notes b.md >/dev/null
ok "topic changes the overlay" l save -a -m "topic notes" -q
cd $M
echo mainside > notes.md
echo c > c.md
l overlay move notes c.md >/dev/null
ok "main changes the same file" l save -a -m "main notes" -q
out=$(l merge topic 2>&1)
has "the merge names the overlay" "Merged overlays: notes" "$out"
eq "the incoming file wins" topic "$(cat notes.md)"
eq "the files of both sides are there" "b.md c.md notes.md" "$(l overlay ls-files notes | tr '\n' ' ' | sed 's/ $//')"
eq "no conflicts" "" "$(l status --short)"
# The automatic states that keep the working content of the view of notes (docs/overlays.md section 1)
# are no states of its history.
autos() { for id in $(sqlite3 "$O/$1/meta/db/layr.sqlite" "select json_extract(value, '\$') from config where key like 'auto.%'"); do [ -e "$O/$1/states/$id" ] && echo "$id"; done | wc -l | tr -d ' '; }
eq "history=last keeps the first state and two line heads" 3 "$(( $(states notes) - $(autos notes) ))"

# The same change with three-way: a conflict (the control for the union).
ok "a three-way overlay above notes" l overlay create tw --parent notes -q
echo t1 > tw.md
l overlay move tw tw.md >/dev/null
ok "first three-way state" l save -m tw1 -q
l line new topic2 main >/dev/null
(cd $LAYR_ROOT/projects/p/lines/topic2 && echo theirs > tw.md && l save -a -m t2 -q)
echo ours > tw.md
ok "main changes it too" l save -a -m t3 -q
l merge topic2 >/dev/null 2>&1
has "three-way gives a conflict" "tw.md" "$(l status --short)"
l restore --source head tw.md >/dev/null 2>&1 || echo ours > tw.md

# 4. quiet: the files are out of status, diff and add; a named save takes them as they are.
echo k1 > .env
ok "an overlay above tw" l overlay create keys --parent tw -q
l config overlay.keys.quiet true >/dev/null
has "with the old setting quiet" "quiet" "$(l overlay list | grep '^. keys')"
l overlay move keys .env >/dev/null
ok "the move saves" l save -m keys -q
echo k2 > .env
eq "status leaves the quiet file out" "" "$(l status --short | grep .env)"
eq "status --json too" "[]" "$(l status --json | python3 -c 'import json,sys; s=json.load(sys.stdin); print([c["path"] for c in s["unstaged"]+s["staged"] if c["path"]==".env"])')"
eq "diff leaves it out" "" "$(l diff -- .env)"
l add -A >/dev/null 2>&1
eq "add -A does not stage it" "" "$(l diff --cached --name-only | grep .env)"
ok "a named save with only the quiet change" l save -m "quiet only" -q
eq "the overlay has the working content" k2 "$(l overlay show keys head:.env)"
echo x >> notes.txt
l add notes.txt
echo k3 > .env
ok "a named save with both, the shared file to main" l save -m both --to main -q
eq "the quiet file follows" k3 "$(l overlay show keys head:.env)"
eq "the shared state has only the shared file" "notes.txt" "$(l show head --name-only --format= | grep .)"
ok "the old setting can be taken away" l config --unset overlay.keys.quiet
echo k4 > .env
has "then status shows it" ".env" "$(l status --short)"

finish
