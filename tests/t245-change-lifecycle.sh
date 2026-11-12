#!/bin/bash
# ci: macos overlayfs
# Change numbers, titles and closing (docs/review.md, "Change numbers, titles and closing"). The project creator gives
# each change a number (`#12`); another machine shows the number after it gets the creator's record. `change edit`
# sets a title and a description apart from the save message; the last edit wins on every machine. `change close`
# hides a change from `change list`, `queue add` refuses it and `queue run` fails its pending entry.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

js() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
number_of() { l change list --all --json | js "next(c['number'] for c in d if c['change_id']=='$1')"; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
l line new -q topic
cd "$P/topic"
echo one > one.txt
l add one.txt
l save -q -m "feat(topic): add one

The first line of a change."
C=$(l log -n1 --json | head -1 | js "d['change_id']")
N=$(number_of "$C")
case "$N" in '' | None | *[!0-9]*) N= ;; esac
ok "the creator machine gives the change a number" test -n "$N"
eq "the number resolves with #" "$C" "$(l change show "#$N" --json | js "d['change_id']")"
eq "a bare number resolves too" "$C" "$(l change show "$N" --json | js "d['change_id']")"
eq "changes/#N is a revision" "$(l rev-parse head)" "$(l rev-parse "changes/#$N")"
eq "show --json has the number" "$N" "$(l change show "#$N" --json | js "d['number']")"
no "#0 is no change number" l change show '#0'
has "an unknown number is refused" "unknown change" "$(l change show '#999' 2>&1)"
eq "the title falls back to the first message line" "feat(topic): add one" "$(l change show "#$N" --json | js "d['title']")"
eq "the description falls back to the message body" "The first line of a change." "$(l change show "#$N" --json | js "d['description']")"
eq "a new change is open" open "$(l change show "#$N" --json | js "d['status']")"
eq "the import on main is merged" merged "$(l change list --json | js "[c['status'] for c in d if c['change_id']!='$C'][0]")"

ok "edit the title and the description" l change edit "#$N" --title "Add the first file" --description "Why: a test."
eq "the edited title shows in JSON" "Add the first file" "$(l change show "#$N" --json | js "d['title']")"
eq "the edited description shows in JSON" "Why: a test." "$(l change show "#$N" --json | js "d['description']")"
has "change list shows the edited title" "#$N	$C	v1	open	Add the first file" "$(l change list)"
printf 'Line one.\nLine two.\n' > "$T/desc.txt"
ok "a description from a file" l change edit "#$N" --description-file "$T/desc.txt"
eq "the file sets the description" "Line one.|Line two." "$(l change show "#$N" --json | js "d['description'].replace(chr(10),'|')")"
eq "an edit of the description keeps the title" "Add the first file" "$(l change show "#$N" --json | js "d['title']")"
no "a title has one line" l change edit "#$N" --title "$(printf 'a\nb')"
no "edit needs a field" l change edit "#$N"
no "a description and a file are refused together" l change edit "#$N" --description x --description-file "$T/desc.txt"

ok "close the change with a reason" l change close "#$N" -m "not needed"
eq "the change is closed" closed "$(l change show "#$N" --json | js "d['status']")"
eq "the reason is recorded" "not needed" "$(l change show "#$N" --json | js "d['closed']['reason']")"
has "change show tells who closed it" "closed by" "$(l change show "#$N")"
no "change list hides a closed change" sh -c "'$LAYR' change list | grep -q '$C'"
has "change list --closed shows it" "$C" "$(l change list --closed)"
has "change list --all shows it" "$C" "$(l change list --all)"
eq "change list --closed shows only closed changes" 1 "$(l change list --closed --json | js "len(d)")"
has "closing again changes nothing" "closed already" "$(l change close "#$N")"
has "queue add refuses a closed change" "is closed" "$(l queue add topic --into main --no-run-queue 2>&1)"
no "queue add exits non-zero for a closed change" l queue add topic --into main --no-run-queue

ok "reopen the change" l change reopen "#$N"
eq "the change is open again" open "$(l change show "#$N" --json | js "d['status']")"
ok "queue add takes the open change" l queue add topic --into main --no-run-queue
ok "close it while the entry waits" l change close "#$N"
cd "$P/main"
l queue run --into main >/dev/null 2>&1
eq "queue run fails the entry of a closed change" failed "$(l queue list --into main --json | js "d[-1]['status']")"
has "the reason names the closed change" "#$N is closed" "$(l queue list --into main --json | js "d[-1]['reason']")"
eq "main did not take the closed change" "" "$(ls one.txt 2>/dev/null)"
eq "no hold stays for the failed entry" "[]" "$(l ref list holds/ --json)"

M=$(l change list --json | js "[c['number'] for c in d if c['status']=='merged'][0]")
has "a merged change cannot be closed" "merged" "$(l change close "#$M" 2>&1)"
eq "change numbering names this machine" True "$(l change numbering --json | js "d['this_machine']")"

# Two machines: the clone shows the creator's numbers, has none for its own new change until the creator saw it,
# and both machines agree on the title after concurrent edits.
ok "reopen for the second machine" l change reopen "#$N"
ok "remote add" l remote add cloud "file://$T/plain"
ok "push to the store" l push cloud -q
FIRST=$LAYR_ROOT
new_fs $T/second 2G
export LAYR_ROOT=$T/second/store
cd "$T"
ok "clone the store" l clone "file://$T/plain" copy -q
cd "$LAYR_ROOT/projects/copy/lines/main"
read -r SECOND_MACHINE SECOND_KEY < <(l remote list --json | python3 -c 'import json,sys; m=next(m for m in json.load(sys.stdin)["machines"] if m["current"]); print(m["id"], m["key"])')
eq "the clone reads the creator's number" "$N" "$(number_of "$C")"
eq "the clone reads the edited title" "Add the first file" "$(l change show "#$N" --json | js "d['title']")"
eq "the clone is not the numbering machine" False "$(l change numbering --json | js "d['this_machine']")"
echo second >> notes.txt
l save -q -a -m "fix: second machine"
C2=$(l log -n1 --json | head -1 | js "d['change_id']")
eq "a change of the clone has no number yet" None "$(number_of "$C2")"
ok "the author edits the title on the second machine" l change edit "$C2" --title "title of the second machine"
ok "push from the clone" l push origin -q

export LAYR_ROOT=$FIRST
cd "$P/main"
ok "trust the second machine" l remote trust cloud "$SECOND_MACHINE" --public-key "$SECOND_KEY"
ok "fetch its records" l fetch cloud -q
N2=$(number_of "$C2")
eq "the creator numbers the fetched change next" "$((N + 1))" "$N2"
sleep 0.01
ok "a maintainer edits the same title later" l change edit "#$N2" --title "title of the first machine"
ok "push the number and the edit" l push cloud -q

export LAYR_ROOT=$T/second/store
cd "$LAYR_ROOT/projects/copy/lines/main"
ok "fetch on the second machine" l fetch origin -q
eq "the second machine shows the same number" "$N2" "$(number_of "$C2")"
eq "the later edit wins on the second machine" "title of the first machine" "$(l change show "#$N2" --json | js "d['title']")"

export LAYR_ROOT=$FIRST
cd "$P/main"
eq "the later edit wins on the first machine" "title of the first machine" "$(l change show "#$N2" --json | js "d['title']")"

# The creator hands the numbering role to the second machine: that machine numbers its own changes from then on.
has "a machine that does not give numbers cannot hand the role" "only the numbering machine" "$(cd "$T/second/store/projects/copy/lines/main" && LAYR_ROOT=$T/second/store "$LAYR" change numbering --hand-to "$SECOND_MACHINE" 2>&1)"
ok "hand the numbering role to the second machine" l change numbering --hand-to "$SECOND_MACHINE"
eq "the first machine no longer gives numbers" False "$(l change numbering --json | js "d['this_machine']")"
ok "push the handoff" l push cloud -q
export LAYR_ROOT=$T/second/store
cd "$LAYR_ROOT/projects/copy/lines/main"
ok "fetch the handoff" l fetch origin -q
eq "the second machine gives numbers now" True "$(l change numbering --json | js "d['this_machine']")"
echo third >> notes.txt
l save -q -a -m "fix: third change"
C3=$(l log -n1 --json | head -1 | js "d['change_id']")
eq "the second machine numbers its new change next" "$((N2 + 1))" "$(number_of "$C3")"
finish
