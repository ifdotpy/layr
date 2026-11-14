#!/bin/bash
# ci: macos overlayfs
# Deletion checks the current line, or main without a current line, using fresh saved heads.
. "$(dirname "$0")/lib.sh"

wait_for_delete() {
    for _ in $(seq 1 500); do
        [ -f "$barrier/ready" ] && return
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.02
    done
    touch "$barrier/release"
    wait "$pid"
    cat "$T/race.out"
    echo 'FATAL: line delete did not reach the barrier'
    exit 1
}

sample "$T/src"
l init del --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/del/lines
cd "$P/main"
l line new -q empty main || exit 1
ok "a base-only line deletes without a current line" l --project del line delete empty
l line new -q unrelated main || exit 1
l line new -q ff main || exit 1
cd "$P/ff"
printf 'fast-forward\n' > ff.txt
l add ff.txt && l save -qm ff || exit 1
ff=$(l rev-parse head)
cd "$P/main"
l merge -q ff --expect "$ff" || exit 1
eq "fast-forward heads are equal" "$ff" "$(l rev-parse head)"
out=$(l --project del --line unrelated line delete ff 2>&1); rc=$?
eq "an explicit unrelated destination refuses deletion" 1 "$rc"
has "the explicit destination is named" "not fully merged into 'unrelated'" "$out"
# Keep cwd outside the deleted line: OverlayFS cannot unmount a working directory.
cd "$T"
ok "project without --line uses main without a current line" l --project del line delete ff
cd "$P/main"
no "the fast-forward line is deleted" test -e "$P/ff"

# A merge commit contains the topic through its second parent.
l line new -q topic main || exit 1
printf 'main\n' > main.txt
l add main.txt && l save -qm main || exit 1
cd "$P/topic"
printf 'topic\n' > topic.txt
l add topic.txt && l save -qm topic || exit 1
cd "$P/main"
l merge -q topic || exit 1
eq "ordinary merge has two parents" 2 "$(l show -s --format=%p head | wc -w | tr -d ' ')"
cd "$T"
ok "ordinary merge ancestry deletes without a current line" l --project del line delete topic

# --discard only allows unsaved files; it does not allow unmerged named saves.
l --project del line new -q pending main || exit 1
cd "$P/pending"
printf 'pending\n' > pending.txt
l add pending.txt && l save -qm pending || exit 1
cd "$T"
out=$(l --project del line delete pending 2>&1); rc=$?
eq "an unmerged line is refused" 1 "$rc"
has "the fallback destination is named" "not fully merged into 'main'" "$out"
out=$(l --project del line delete --discard pending 2>&1); rc=$?
eq "--discard still checks mergedness" 1 "$rc"
has "--discard names the destination" "not fully merged into 'main'" "$out"
printf 'unsaved\n' > "$P/pending/new.txt"
out=$(l --project del line delete -D pending 2>&1); rc=$?
eq "-D still refuses unsaved changes" 1 "$rc"
has "the unsaved safeguard remains" "line 'pending' has unsaved changes" "$out"
ok "-D --discard skips mergedness and discards unsaved work" l --project del line delete -D --discard pending

# The fallback must never make main its own proof of mergedness.
out=$(l --project del line delete main 2>&1); rc=$?
eq "the fallback cannot compare main with itself" 1 "$rc"
has "the self-target error names main" "deletion target 'main' is the same line" "$out"
out=$(l --project del --line main line delete -D main 2>&1); rc=$?
eq "-D still cannot delete the current line" 1 "$rc"
has "the current-line safeguard remains" "used as the current folder" "$out"

# Revalidate both heads after preparation, under the deletion lock. Wait for readiness before
# changing a head, so an initial-check refusal cannot make either race pass.
cd "$P/main"
l line new -q race main || exit 1
cd "$P/race"
printf 'race\n' > race.txt
l add race.txt && l save -qm race || exit 1
cd "$P/main"
l merge -q race || exit 1
barrier=$T/destination-barrier
mkdir "$barrier"
LAYR_TEST_LINE_DELETE_BARRIER="$barrier" l --project del line delete race >"$T/race.out" 2>&1 &
pid=$!
wait_for_delete
l reset -q --hard head~1 || { touch "$barrier/release"; wait "$pid"; exit 1; }
touch "$barrier/release"
wait "$pid"; rc=$?
eq "a destination rewound before the lock refuses deletion" 1 "$rc"
has "the locked check names main" "not fully merged into 'main'" "$(cat "$T/race.out")"
ok "the line survives the destination race" test -d "$P/race"
l merge -q race || exit 1
barrier=$T/source-barrier
mkdir "$barrier"
LAYR_TEST_LINE_DELETE_BARRIER="$barrier" l --project del line delete race >"$T/race.out" 2>&1 &
pid=$!
wait_for_delete
cd "$P/race"
printf 'late\n' > late.txt
l add late.txt && l save -qm late || { touch "$barrier/release"; wait "$pid"; exit 1; }
cd "$P/main"
touch "$barrier/release"
wait "$pid"; rc=$?
eq "a source advanced before the lock refuses deletion" 1 "$rc"
has "the advanced source is still unmerged" "not fully merged into 'main'" "$(cat "$T/race.out")"
ok "the source save survives the race" test -f "$P/race/late.txt"

# A missing fallback gives a useful error only when mergedness actually needs it.
l line new -q empty main || exit 1
l line rename main trunk >/dev/null || exit 1
cd "$T"
out=$(l --project del line delete race 2>&1); rc=$?
no "a missing main refuses deletion" test "$rc" -eq 0
has "the missing fallback is explained" "default deletion target 'main' is unavailable" "$out"
has "the underlying missing-line error is preserved" "line 'main' not found" "$out"
ok "a base-only line needs no main" l --project del line delete empty
ok "-D needs no main" l --project del line delete -D race
l --project del --line trunk line new -q forced trunk || exit 1
cd "$P/forced"
printf 'forced\n' > forced.txt
l add forced.txt && l save -qm forced || exit 1
printf 'unsaved\n' > unsaved.txt
cd "$T"
ok "-f needs no main and allows unmerged saves and unsaved work" l --project del line delete -f forced

finish
