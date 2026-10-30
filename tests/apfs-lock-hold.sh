#!/bin/bash
# Short store lock holds on APFS: a command saves the working folder before the lock (the
# lock only takes that layer), and the state after the command copies only what it changed.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

sample "$T/src"
l init hold --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/hold/lines
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'tracked\n' > tracked.txt
l add tracked.txt && l save -qm tracked
mkdir -p bulk
for i in $(seq 1 300); do printf '%s\n' "$i" > "bulk/f$i"; done
printf 'edited\n' > tracked.txt

trace=$(LAYR_TRACE=1 l restore tracked.txt 2>&1)
eq "restore discards the edit" "tracked" "$(cat tracked.txt)"
has "the save before the command was written before the lock" "take prepared save" "$trace"
has "the state after the command copies only the restored file" "thin write 1 " "$(printf '%s\n' "$trace" | sed 's/  */ /g')"
ok "undo of the restore" l undo
eq "undo brings the edit back" "edited" "$(cat tracked.txt)"
eq "the untracked files stay" "300" "$(ls bulk | wc -l | tr -d ' ')"
eq "no prepared copies stay" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"

# restore copies only the named paths, and keeps the meaning of a folder and a deleted file.
mkdir -p dir && printf 'one\n' > dir/a && printf 'two\n' > dir/b
l add dir && l save -qm dir
printf 'changed\n' > dir/a && rm dir/b && printf 'untracked\n' > dir/c && rm tracked.txt
ok "restore a folder and a deleted file" l restore dir tracked.txt
eq "a changed file in the folder comes back" "one" "$(cat dir/a)"
eq "a deleted file in the folder comes back" "two" "$(cat dir/b)"
eq "an untracked file in the folder stays" "untracked" "$(cat dir/c)"
eq "a deleted named file comes back from the index" "tracked" "$(cat tracked.txt)"
rm dir/c

# add reads the working folder before the lock and does not read it again under the lock.
printf 'new\n' > new.txt
eq "add reads the folder once" "1" "$(LAYR_TRACE=1 l add new.txt 2>&1 | grep -c 'validated read')"
has "add stages the file" "A  new.txt" "$(l status --short)"

# save -a -m stages the changed tracked files with the read of add, before the lock.
printf 'changed by -a\n' > dir/a
trace=$(LAYR_TRACE=1 l save -a -qm all 2>&1)
eq "save -a reads the folder once" "1" "$(printf '%s\n' "$trace" | grep -c 'validated read')"
eq "save -a saves the changed tracked file" "changed by -a" "$(l show head:dir/a)"
no "save -a leaves the file unstaged" sh -c "'$LAYR' status --short | grep -q 'dir/a'"

# git push talks to the remote without the store lock: another command runs during a slow push.
R=$T/remote.git
git init -q --bare "$R"
printf '#!/bin/sh\nsleep 6\n' > "$R/hooks/pre-receive" && chmod +x "$R/hooks/pre-receive"
l save -qm staged
ok "add the remote" l git remote add slow "$R"
l git push -q slow topic > "$T/push.log" 2>&1 &
PUSH=$!
sleep 2
start=$(date +%s)
ok "a command runs during the push" l line new -q during-push main
took=$(( $(date +%s) - start ))
ok "it did not wait for the push" test "$took" -lt 3
wait $PUSH
eq "the push succeeded" "0" "$?"
ok "the remote has the line" git -C "$R" rev-parse --verify -q topic
# A folder that changes while a save waits for the lock: the layer written before the lock
# becomes an automatic state, and the state under the lock copies only the changed entry.
mkdir -p bulk2 && for i in $(seq 1 200); do printf '%s\n' "$i" > "bulk2/f$i"; done
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"a"); fcntl.flock(f,fcntl.LOCK_EX); time.sleep(6)' "$LAYR_ROOT/projects/hold/meta/lock" &
HOLDER=$!
sleep 1
LAYR_TRACE=1 l save > "$T/late-save.log" 2>&1 &
SAVE=$!
sleep 3
printf 'late\n' > late.txt
wait $HOLDER; wait $SAVE
trace=$(cat "$T/late-save.log")
has "the save wrote its layer before the lock" "prepare save" "$trace"
has "the changed folder gets a layer over the prepared one" "layer over prepared save" "$trace"
has "under the lock only the changed entry is copied" "thin write 1 entries" "$(printf '%s\n' "$trace" | sed 's/  */ /g')"
auto=$(printf '%s\n' "$trace" | awk '$1 == "topic" {print $2}')
eq "the state has the late file" "late" "$(l show "$auto:late.txt")"
eq "the state has the files of the prepared layer" "200" "$(l show "$auto:bulk2/f200")"
rm -rf bulk2 late.txt

# A save after a save copies only what changed since the previous save, not every change since
# the head (a new node_modules at each save), and a save with no change writes no layer.
mkdir -p bulk3 && for i in $(seq 1 200); do printf '%s\n' "$i" > "bulk3/f$i"; done
l save >/dev/null
printf 'changed\n' > bulk3/f1
trace=$(LAYR_TRACE=1 l save 2>&1)
eq "the next save copies only the changed file" "1" "$(printf '%s\n' "$trace" | sed -n 's/.*thin write \([0-9]*\) entries/\1/p' | sort -n | tail -1)"
auto=$(printf '%s\n' "$trace" | awk '$1 == "topic" {print $2}')
eq "the state has the change" "changed" "$(l show "$auto:bulk3/f1")"
eq "the state keeps the files of the earlier save" "200" "$(l show "$auto:bulk3/f200")"
eq "a save without a change writes no layer" "" "$(LAYR_TRACE=1 l save 2>&1 | grep 'thin write')"
rm -rf bulk3

# merge and reset --hard copy and merge the trees before the lock; under it they only rename.
locked() { printf '%s\n' "$1" | awk '/store lock held/{exit} on{print} /store lock taken/{on=1}'; }
copies='APFS (clone|copy tree)|thin materialize'
l line new -q side
cd "$P/side" || exit 1
printf 'side\n' > side.txt && l add side.txt && l save -qm side
cd "$P/topic" || exit 1
printf 'topic\n' > topic.txt && l add topic.txt && l save -qm topic
trace=$(LAYR_TRACE=1 l merge -q side 2>&1)
has "merge prepares before the lock" "prepare merge" "$trace"
has "merge takes the lock" "store lock taken" "$trace"
eq "merge copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "the merge brings the file" "side" "$(cat side.txt)"
trace=$(LAYR_TRACE=1 l reset -q --hard side 2>&1)
has "reset takes the lock" "store lock taken" "$trace"
eq "reset --hard copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
no "reset --hard removes a file that the target does not have" test -e topic.txt
eq "reset --hard keeps untracked files" "300" "$(ls bulk | wc -l | tr -d ' ')"
eq "no prepared copies stay" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
# replay merges each state onto the new base before the lock; under it each step takes its result.
l line new -q rp-base main
l line new -q rp-work main
cd "$P/rp-work" || exit 1
printf 'one\n' > rp1.txt && l add rp1.txt && l save -qm rp1
printf 'two\n' > rp2.txt && l add rp2.txt && l save -qm rp2
cd "$P/rp-base" || exit 1
printf 'base\n' > rpb.txt && l add rpb.txt && l save -qm rpb
cd "$P/rp-work" || exit 1
trace=$(LAYR_TRACE=1 l replay rp-base 2>&1)
has "replay merges before the lock" "prepare replay" "$trace"
has "replay takes the lock" "store lock taken" "$trace"
eq "replay copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "the replayed line has every file" "base one two" "$(cat rpb.txt rp1.txt rp2.txt | tr '\n' ' ' | sed 's/ $//')"
eq "the replayed states keep their messages" "rp2 rp1 rpb" "$(l log -3 --format=%s | tr '\n' ' ' | sed 's/ $//')"
eq "no prepared copies stay after replay" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"

# clean -n only reads; clean -f, park and pick copy before the lock; undo reads its trees before it.
mkdir -p junk && for i in $(seq 1 50); do printf '%s\n' "$i" > "junk/f$i"; done
trace=$(LAYR_TRACE=1 l clean -n -d 2>&1)
has "clean -n lists the folder" "Would remove junk/" "$trace"
eq "clean -n takes no store lock" "" "$(printf '%s\n' "$trace" | grep 'store lock taken')"
trace=$(LAYR_TRACE=1 l clean -f -d -q 2>&1)
has "clean -f takes the lock" "store lock taken" "$trace"
eq "clean -f copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
no "clean -f removes the folder" test -e junk
printf 'edit\n' > rp1.txt
trace=$(LAYR_TRACE=1 l park -q 2>&1)
has "park takes the lock" "store lock taken" "$trace"
eq "park copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "park puts the head content back" "one" "$(cat rp1.txt)"
ok "park pop" l park pop -q
eq "park pop brings the edit back" "edit" "$(cat rp1.txt)"
l restore rp1.txt
l line new -q pk-src rp-base
cd "$P/pk-src" || exit 1
printf 'picked\n' > pk.txt && l add pk.txt && l save -qm pk
pk=$(l rev-parse head)
cd "$P/rp-work" || exit 1
trace=$(LAYR_TRACE=1 l pick "$pk" 2>&1)
has "pick merges before the lock" "prepare pick" "$trace"
eq "pick copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "pick brings the file" "picked" "$(cat pk.txt)"
trace=$(LAYR_TRACE=1 l undo 2>&1)
has "undo takes the lock" "store lock taken" "$trace"
eq "undo builds no state tree under the lock" "" "$(locked "$trace" | grep 'thin materialize')"
no "undo of the pick removes the file" test -e pk.txt
eq "no prepared copies stay after these commands" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
eq "the trash is empty" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
ok "states remain valid" l fsck
finish
