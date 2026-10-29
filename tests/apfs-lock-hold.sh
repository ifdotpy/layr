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
ok "states remain valid" l fsck
finish
