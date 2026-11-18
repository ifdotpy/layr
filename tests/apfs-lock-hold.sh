#!/bin/bash
# Short store lock holds on APFS: a command saves the working folder before the lock (the
# lock only publishes it), and the state after the command copies only what it changed.
[ "$(uname -s)" = Darwin ] || { echo 'This test requires macOS'; exit 1; }
. "$(dirname "$0")/lib.sh"

# Extract every project lock hold: retries must also copy outside the lock.
locked() { printf '%s\n' "$1" | awk '/store lock held/{on=0} on{print} /store lock taken/{on=1}'; }
copies='APFS (clone|copy tree)|thin materialize'
sample "$T/src"
l init hold --no-objects --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/hold/lines
cd "$P/main" || exit 1
l line new -q topic
cd "$P/topic" || exit 1
printf 'tracked\n' > tracked.txt
l add tracked.txt && l save -qm tracked
mkdir -p bulk
for i in $(seq 1 300); do printf '%s\n' "$i" > "bulk/f$i"; done
printf 'edited\n' > tracked.txt
# A prepared save trusts only entries a full second older than its observation (racy files).
sleep 1.1

trace=$(LAYR_TRACE=1 l restore tracked.txt 2>&1)
eq "restore discards the edit" "tracked" "$(cat tracked.txt)"
has "the save before the command is prepared" "prepare save" "$trace"
eq "the save before the command copies outside the lock" "" "$(locked "$trace" | grep -E "$copies|thin write")"
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
# The fs stack validates the read of add in two steps (the status and the staged copy), both before the lock.
reads=$(LAYR_TRACE=1 l add new.txt 2>&1 | grep -cE 'validated (filesystem )?read')
ok "add reads the folder before the lock, at most twice ($reads)" test "$reads" -ge 1 -a "$reads" -le 2
has "add stages the file" "A  new.txt" "$(l status --short)"

# save -a -m stages the changed tracked files with the read of add, before the lock. The named save
# prepares its left-out comparison before the lock; the warning verifies concurrent writes (t96).
printf 'changed by -a\n' > dir/a
trace=$(LAYR_TRACE=1 l save -a -qm all 2>&1)
reads=$(printf '%s\n' "$trace" | grep -cE 'validated (filesystem )?read')
# A read that finds no racy entry is reused by the warning preparation: one or two reads.
ok "save -a reads the folder for add and for its warning preparation ($reads)" test "$reads" -ge 1 -a "$reads" -le 2
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
# is discarded and the latest folder is captured again outside the project lock.
mkdir -p bulk2 && for i in $(seq 1 200); do printf '%s\n' "$i" > "bulk2/f$i"; done
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"a"); fcntl.flock(f,fcntl.LOCK_EX); time.sleep(12)' "$LAYR_ROOT/projects/hold/meta/lock" &
HOLDER=$!
sleep 1
LAYR_TRACE=1 l save > "$T/late-save.log" 2>&1 &
SAVE=$!
# Change the folder only after the save wrote its layer and waits for the lock (a busy runner can take seconds).
for i in $(seq 1 100); do grep -q "waiting for the project lock" "$T/late-save.log" 2>/dev/null && break; sleep 0.1; done
printf 'late\n' > late.txt
wait $HOLDER; wait $SAVE
trace=$(cat "$T/late-save.log")
has "the save wrote its layer before the lock" "prepare save" "$trace"
ok "the changed folder is prepared again" test "$(printf '%s\n' "$trace" | grep -c 'prepare save')" -ge 2
eq "the late edit is captured outside the lock" "" "$(locked "$trace" | grep -E "$copies|thin write")"
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
# Both already-up-to-date paths must prepare a dirty automatic-save layer before the lock.
head=$(l rev-parse head)
printf 'dirty up-to-date\n' > tracked.txt
sleep 1.1  # not racy for the prepared save
trace=$(LAYR_TRACE=1 l merge -q side 2>&1)
has "an up-to-date merge publishes its prepared save" "publish prepared save" "$trace"
eq "an up-to-date merge copies no layer under the lock" "" "$(locked "$trace" | grep -E "$copies|thin write")"
eq "an up-to-date merge keeps dirty content" "dirty up-to-date" "$(cat tracked.txt)"
eq "an up-to-date merge keeps the head" "$head" "$(l rev-parse head)"
eq "no prepared copies stay after an up-to-date merge" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"

# The source selects an empty follow overlay; it changes no target layer, but goes through
# the overlays-only up-to-date branch. Keep the target without overlays so its save is thin.
# Use a separate project: new lines show every readable project overlay, even when their
# source shows none. Leaving this overlay in hold would change the replay, clean and park tests.
l init hold-layered --from "$T/src" -q >/dev/null || exit 1
Q=$LAYR_ROOT/projects/hold-layered
cd "$Q/lines/main" || exit 1
printf 'tracked\n' > tracked.txt
l add tracked.txt && l save -qm tracked || exit 1
head=$(l rev-parse head)
l line new -q already-layered || exit 1
cd "$Q/lines/already-layered" || exit 1
l overlay create lock-empty -q || exit 1
l overlay use lock-empty >/dev/null || exit 1
eq "the layered source is already up to date" "$head" "$(l rev-parse head)"
cd "$Q/lines/main" || exit 1
l overlay use --none >/dev/null || exit 1
printf 'dirty layered up-to-date\n' > tracked.txt
sleep 1.1  # not racy for the prepared comparison
trace=$(LAYR_TRACE=1 l merge -q already-layered 2>&1)
has "an up-to-date overlay merge publishes its prepared save" "publish prepared save" "$trace"
eq "an up-to-date overlay merge copies no layer under the lock" "" "$(locked "$trace" | grep -E "$copies|thin write")"
eq "an up-to-date overlay merge keeps dirty content" "dirty layered up-to-date" "$(cat tracked.txt)"
eq "an up-to-date overlay merge keeps the head" "$head" "$(l rev-parse head)"
eq "no prepared copies stay after an up-to-date overlay merge" "" "$(ls -A "$Q/meta/work" 2>/dev/null)"
cd "$P/topic" || exit 1
trace=$(LAYR_TRACE=1 l reset -q --hard side 2>&1)
has "reset takes the lock" "store lock taken" "$trace"
eq "reset --hard copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
no "reset --hard removes a file that the target does not have" test -e topic.txt
eq "reset --hard keeps untracked files" "300" "$(ls bulk | wc -l | tr -d ' ')"
eq "no prepared copies stay" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
# status compares the working folder without the store lock: the scan and the compare come
# before any lock, and a status with no staged change takes none.
cd "$P/topic" || exit 1
printf 'dirty\n' > dirty.txt
trace=$(LAYR_TRACE=1 l status 2>&1)
has "status compares the folder" "status compare" "$trace"
eq "status compares nothing under the store lock" "" "$(locked "$trace" | grep -E 'status compare|scan|validated read')"
eq "status holds no store lock" "" "$(printf '%s\n' "$trace" | grep 'store lock taken')"
rm -f dirty.txt
# replay merges each state onto the new base before the lock; under it each step takes its result.
l line new -q rp-base main
l line new -q rp-work main
cd "$P/rp-work" || exit 1
printf 'one\n' > rp1.txt && l add rp1.txt && l save -qm rp1
printf 'two\n' > rp2.txt && l add rp2.txt && l save -qm rp2
cd "$P/rp-base" || exit 1
printf 'base\n' > rpb.txt && l add rpb.txt && l save -qm rpb
cd "$P/rp-work" || exit 1
eq "the replay fixture shows no overlays" "line rp-work shows no overlays" "$(l overlay use)"
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
# A pick without a merge (t113) writes a thin layer under the lock; any other pick merges before it.
case "$trace" in
    *"pick visits"*) has "pick takes the thin path" "pick visits" "$trace" ;;
    *) has "pick merges before the lock" "prepare pick" "$trace" ;;
esac
eq "pick copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "pick brings the file" "picked" "$(cat pk.txt)"
trace=$(LAYR_TRACE=1 l undo 2>&1)
has "undo takes the lock" "store lock taken" "$trace"
eq "undo builds no state tree under the lock" "" "$(locked "$trace" | grep 'thin materialize')"
no "undo of the pick removes the file" test -e pk.txt
eq "no prepared copies stay after these commands" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
# line new clones the tree, makes .git and takes caches before the lock; under it only renames.
cd "$P/main" || exit 1
for opt in "" "--count 2"; do
    trace=$(LAYR_TRACE=1 l line new $opt -q nl${opt:+b} 2>&1)
    has "line new $opt takes the lock" "store lock taken" "$trace"
    has "line new $opt prepares before the lock" "prepare lines" "$trace"
    eq "line new $opt copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies|make .git|git folder|take cache")"
    eq "line new $opt runs no preparation under the lock" "" "$(locked "$trace" | grep -E 'prepare lines')"
done
ok "the new lines exist" test -d "$P/nl" -a -d "$P/nlb-1" -a -d "$P/nlb-2"
no "a taken name is refused" l line new -q nl
eq "no prepared copies stay after line new" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
# The trash is emptied in the background after a command.
for i in $(seq 1 100); do [ -z "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)" ] && break; sleep 0.1; done
eq "the trash is empty" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
ok "states remain valid" l fsck

# gc reads the thin layers a fixed number of times, however many states it deletes, and leaves no trash.
cd "$P/main" || exit 1
l config retention.auto 2; l config undo.ops 0
for i in 1 2 3 4 5 6 7 8; do echo "$i" > gcauto.txt; l save -q; done
trace=$(LAYR_TRACE=1 l gc --repair 2>&1)
n=$(printf '%s\n' "$trace" | grep -c 'thin bases')
ok "gc walks the layers a fixed number of times, not once per deleted state" test "$n" -ge 1 -a "$n" -le 4
eq "the trash is empty after gc" "" "$(ls -A "$LAYR_ROOT/trash" 2>/dev/null)"
ok "states remain valid after gc" l fsck
# save --overlays converts a plain cache folder and clones the caches before the lock; under it
# only checks and records. The first save (plain folder) and a repeated one (new head).
cd "$P/topic" || exit 1
printf 'cachedir/\n' >> .gitignore
printf 'cachedir regenerable\n' > .layrattributes
l add .gitignore .layrattributes && l save -qm cache-attrs
mkdir -p cachedir/sub && for i in $(seq 1 200); do printf '%s\n' "$i" > "cachedir/sub/f$i"; done
trace=$(LAYR_TRACE=1 l save --overlays --folder cachedir 2>&1)
eq "save --overlays copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
has "save --overlays clones before the lock" "APFS clone" "$trace"
eq "the cache keeps its files" "200" "$(ls cachedir/sub | wc -l | tr -d ' ')"
printf 'x\n' > cache-head.txt
l add cache-head.txt && l save -qm cache-head
printf 'more\n' > cachedir/more
trace=$(LAYR_TRACE=1 l save --overlays --folder cachedir 2>&1)
eq "a repeated save --overlays copies no tree under the lock" "" "$(locked "$trace" | grep -E "$copies")"
eq "no prepared copies stay after save --overlays" "" "$(ls -A "$LAYR_ROOT/projects/hold/meta/work" 2>/dev/null)"
ok "states remain valid after save --overlays" l fsck
finish
