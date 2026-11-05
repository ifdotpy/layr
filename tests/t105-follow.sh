#!/bin/bash
# ci: macos overlayfs
# layr follow (docs/follow.md): a line keeps a remote branch. A pass fast-forwards main; unsaved
# work skips it, an untracked file does not; a conflicting remote change leaves main unchanged and the error shows in
# `layr follow`; the caches of a line that was squash-merged on the remote come to main; `--remove`
# stops it. On macOS the daemon runs the same pass as the user. One user, so it runs on macOS and
# on Linux (the daemon part on macOS only).
. "$(dirname "$0")/lib.sh"
# layr follow probes the user socket: keep it in the test folder, never the real one. macOS reads
# it under HOME; on Linux the tests run as root (direct mode), so LAYR_SOCKET is safe.
mkdir -p $T/home
if [ "$(uname -s)" = Darwin ]; then export HOME=$T/home; else export LAYR_SOCKET=$T/service.sock; fi

R=$T/remote.git; U=$T/up
git init -q --bare -b main $R
git clone -q $R $U 2>/dev/null; cd $U
mkdir -p src; echo base > src/a.txt; echo other > src/b.txt
printf 'target/\n' > .gitignore; echo 'target regenerable' > .layrattributes
git add -A; git commit -qm base; git push -q origin main

l git clone $R gm -q >/dev/null
P=$LAYR_ROOT/projects/gm/lines
cd $P/main

# 1. Set the target: one pass runs now. The remote is main, so nothing changes.
has "layr follow sets the target and reports up to date" "origin/main: up to date" "$(l follow origin/main 2>&1)"
has "layr follow shows the target and the last pass" "Following origin/main on line main" "$(l follow 2>&1)"

# 2. A pass fast-forwards main to a new remote commit.
cd $U; echo one > f1.txt; git add f1.txt; git commit -qm "remote one"; git push -q origin main
cd $P/main
before=$(l rev-parse head)
ok "layr follow --run exits 0" l follow --run
ok "main fast-forwarded: the remote file is in main" test -f f1.txt
ok "main head moved" test "$before" != "$(l rev-parse head)"

# 3. Unsaved work: the pass is skipped and main keeps its head and its work.
cd $U; echo two > f2.txt; git add f2.txt; git commit -qm "remote two"; git push -q origin main
cd $P/main
echo dirty >> src/b.txt
before=$(l rev-parse head)
out=$(l follow --run 2>&1)
has "a line with unsaved work is skipped" "skipped: unsaved work" "$out"
eq "main head did not move with unsaved work" "$before" "$(l rev-parse head)"
no "the remote file did not come with the skipped pass" test -f f2.txt
echo other > src/b.txt
ok "the next pass takes the remote change once the work is clean" l follow --run
ok "main has the second remote file" test -f f2.txt

# 3b. An untracked file is not unsaved work: the pass merges and keeps it out of the state. A remote
#     file at the same path is refused, and the local file stays.
mkdir -p .tool; echo lock > .tool/run.lock
cd $U; echo untracked > f2b.txt; git add f2b.txt; git commit -qm "remote beside untracked"; git push -q origin main
cd $P/main
out=$(l follow --run 2>&1)
no "an untracked file does not skip the pass" grep -q "unsaved work" <<<"$out"
ok "the pass with an untracked file takes the remote change" test -f f2b.txt
eq "the untracked file stays" lock "$(cat .tool/run.lock)"
no "the untracked file is not in the merged state" grep -q "run.lock" <<<"$(l ls-files)"
cd $U; mkdir -p .tool; echo theirs > .tool/run.lock; git add .tool/run.lock; git commit -qm "remote lock"; git push -q origin main
cd $P/main
before=$(l rev-parse head)
l follow --run >/dev/null 2>&1
eq "a remote file at an untracked path does not move main" "$before" "$(l rev-parse head)"
eq "the untracked file keeps the local content" lock "$(cat .tool/run.lock)"
rm -rf .tool
ok "the pass takes the remote file once the local one is gone" l follow --run
eq "main has the remote file at that path" theirs "$(cat .tool/run.lock)"

# 4. Caches of a squash-merged line. feat saves caches and pushes its branch; the remote squash-merges
#    the same change into main. The pass takes the caches of feat into main.
l line new feat main >/dev/null
cd $P/feat
echo feat > feat.txt; l add feat.txt; l save -m "feat work" -q
rm -rf target; mkdir -p target/debug; echo bin-feat > target/debug/x
l save --caches -q
l git push -q origin HEAD:refs/heads/feat >/dev/null 2>&1
cd $U; git pull -q origin main; echo feat > feat.txt; git add feat.txt; git commit -qm "squash feat"; git push -q origin main
cd $P/main
ok "the pass with a squash-merged source succeeds" l follow --run
ok "main has the file of the squash-merged line" test -f feat.txt
eq "main has the caches of the squash-merged line" bin-feat "$(cat target/debug/x 2>/dev/null)"
has "the cache folder is an overlay of main" "target" "$(l overlay list 2>&1)"

# 4b. A source line whose state is not on this machine (evicted, no store copy) refuses only itself: the
#     pass takes the caches of the next source, then builds and saves. good pushes first, gone pushes
#     last, so gone is the first candidate.
dashed() {
    local d=$1 u
    u="${d:20:12}${d:12:8}${d:0:12}"
    printf '%s-%s-%s-%s-%s' "${u:0:8}" "${u:8:4}" "${u:12:4}" "${u:16:4}" "${u:20:12}"
}
for n in good gone; do
    l line new $n main >/dev/null
    cd $P/$n
    echo $n > $n.txt; l add $n.txt; l save -m "$n work" -q
    rm -rf target; mkdir -p target/debug; echo bin-$n > target/debug/x
    l save --caches -q
    l git push -q origin HEAD:refs/heads/$n >/dev/null 2>&1
    cd $P/main
done
# gone saves its caches once more on a state that it did not push.
cd $P/gone; echo more > more.txt; l add more.txt; l save -m "gone more" -q
echo bin-gone-more > target/debug/x; l save --caches -q
# The cache names the automatic state of that save.
gone_head=$(l rev-parse 'head@{0}'); cd $P/main
P_ROOT=$LAYR_ROOT/projects/gm
ok "the cache state of gone was on the disk" test -e "$P_ROOT/states/$(dashed "$gone_head")" -o -e "$P_ROOT/deltas/$(dashed "$gone_head")"
# OverlayFS (Linux) mounts each tree of its registry again when the store opens: drop the entry first.
reg=$LAYR_ROOT/.layr-overlayfs/registry.json
if [ -f "$reg" ]; then
    python3 - "$reg" "$(dashed "$gone_head")" <<'PY'
import json, sys
path, uuid = sys.argv[1], sys.argv[2]
r = json.load(open(path))
r["trees"] = {k: v for k, v in r["trees"].items() if uuid not in k}
json.dump(r, open(path, "w"))
PY
fi
for d in "$P_ROOT/states/$(dashed "$gone_head")" "$P_ROOT/deltas/$(dashed "$gone_head")"; do
    [ -e "$d" ] || continue
    # A saved state on OverlayFS (Linux) is a read-only mount, maybe stacked.
    while command -v mountpoint >/dev/null && mountpoint -q "$d"; do umount "$d" || break; done
    chflags -R nouchg,noschg "$d" 2>/dev/null; chattr -R -i "$d" 2>/dev/null; chmod -R u+w "$d" 2>/dev/null
    rm -rf "$d" 2>/dev/null
    # A read-only btrfs snapshot (Linux) goes only through btrfs.
    [ -e "$d" ] && btrfs property set -ts "$d" ro false 2>/dev/null && btrfs subvolume delete "$d" >/dev/null 2>&1
done
no "the state of gone is gone from the disk" test -e "$P_ROOT/states/$(dashed "$gone_head")" -o -e "$P_ROOT/deltas/$(dashed "$gone_head")"
cd $U; git pull -q origin main; echo good > good.txt; echo gone > gone.txt; echo more > more.txt; git add good.txt gone.txt more.txt
git commit -qm "squash good and gone"; git push -q origin main
cd $P/main
out=$(l follow --run 2>&1)
has "the pass with a missing source state merges" "fast-forward" "$out"
no "a missing source state does not stop the cache step" grep -q "caches not taken" <<<"$out"
eq "main has the caches of the next source" bin-good "$(cat target/debug/x 2>/dev/null)"

# 4c. A head with no cache saved with it (a save after the last cache save, or a pass of an older layr that
#     saved nothing): the up-to-date pass saves the caches, and the next pass does nothing. The remote takes
#     the head, so the pass is up to date.
echo c4 > c4.txt; l add c4.txt; l save -m "local c4" -q
echo bin-main > target/debug/x
l git push -q origin HEAD:refs/heads/main >/dev/null 2>&1
cd $U; git pull -q origin main; cd $P/main
has "an up-to-date pass saves the caches with the head" "up to date; caches saved with the head (target)" "$(l follow --run 2>&1)"
eq "the next pass does nothing" "origin/main: up to date" "$(l follow --run 2>/dev/null)"
eq "the pass keeps the cache files" bin-main "$(cat target/debug/x 2>/dev/null)"

# 4d. A line that pushes over another remote of the same repository (https next to ssh; here file:// next to the
#     path) is a cache source of the pass that follows origin.
l git remote add other "file://$R" >/dev/null 2>&1
l line new viaother main >/dev/null
cd $P/viaother
echo viaother > viaother.txt; l add viaother.txt; l save -m "viaother work" -q
rm -rf target; mkdir -p target/debug; echo bin-viaother > target/debug/x
l save --caches -q
l git push -q other HEAD:refs/heads/viaother >/dev/null 2>&1
cd $U; git pull -q origin main; echo viaother > viaother.txt; git add viaother.txt; git commit -qm "squash viaother"; git push -q origin main
cd $P/main
has "the pass merges the line pushed over the other remote" "fast-forward" "$(l follow --run 2>&1)"
eq "main has the caches of the line pushed over the other remote" bin-viaother "$(cat target/debug/x 2>/dev/null)"

# 5. A conflicting remote change: main keeps its head, and the error shows in layr follow.
echo local > src/a.txt; l save -a -m "local a" -q
cd $U; echo remote > src/a.txt; git commit -qam "remote a"; git push -q origin main
cd $P/main
before=$(l rev-parse head)
out=$(l follow --run 2>&1); rc=$?
eq "a conflicting pass exits 1" "1" "$rc"
eq "main head did not move on a conflict" "$before" "$(l rev-parse head)"
eq "the working file keeps the local content" local "$(cat src/a.txt)"
has "layr follow shows the conflict error" "conflicts: src/a.txt" "$(l follow 2>&1)"
eq "status shows no merge in progress" "" "$(l status -s | grep UU)"

# 5b. A tick (the daemon's pass) does not retry the remote state that the conflict refused: no
#     output, and the last pass keeps its time. A new remote commit is tried again.
last=$(l follow | tail -1)
eq "a tick with the same remote state prints nothing" "" "$(l follow --tick 2>&1)"
eq "a tick with the same remote state does not run the pass" "$last" "$(l follow | tail -1)"
cd $U; echo remote-two > src/a.txt; git commit -qam "remote a again"; git push -q origin main
cd $P/main
out=$(l follow --tick 2>&1); rc=$?
eq "a tick with a new remote state tries again and fails" "1" "$rc"
has "the new attempt names the conflict" "conflicts: src/a.txt" "$out"
eq "the tick of the new remote state is refused again without output" "" "$(l follow --tick 2>&1)"
eq "main head still did not move" "$before" "$(l rev-parse head)"

# 6. Remove the target: no pass runs, and a run without a target fails.
ok "layr follow --remove succeeds" l follow --remove
has "after --remove the line follows nothing" "Not following on line main" "$(l follow 2>&1)"
cd $U; echo three > f3.txt; git add f3.txt; git commit -qm "remote three"; git push -q origin main
cd $P/main
no "layr follow --run without a target fails" l follow --run
no "main did not take the remote file after --remove" test -f f3.txt

# Stop a daemon and wait until the follow pass it forked has ended. A pass that still writes the
# store while the test removes it fails the cleanup. Zombies (state Z) are done; the stopped parent
# cannot reap them.
stop_daemon() {
    kill -STOP "$1" 2>/dev/null
    local n=0 busy=1 c
    while [ $busy -eq 1 ] && [ $n -lt 240 ]; do
        busy=0
        for c in $(pgrep -P "$1" 2>/dev/null); do
            case "$(ps -o stat= -p "$c" 2>/dev/null)" in Z*|"") ;; *) busy=1 ;; esac
        done
        [ $busy -eq 1 ] && sleep 0.5
        n=$((n + 1))
    done
    kill -KILL "$1" 2>/dev/null
    wait "$1" 2>/dev/null
}

# 7. macOS: the daemon runs the pass as the user (socket in the test folder). A fresh clone, so no
#    conflict of step 5 is in its way.
if [ "$(uname -s)" = Darwin ]; then
    l git clone $R gm2 -q >/dev/null
    D=$LAYR_ROOT/projects/gm2/lines/main
    cd $D && l follow origin/main >/dev/null 2>&1
    cd $U; echo four > f4.txt; git add f4.txt; git commit -qm "remote four"; git push -q origin main
    cd $D
    "$LAYR" daemon --tick 2 --socket $T/d.sock > $T/daemon.log 2>&1 &
    dpid=$!
    for i in $(seq 1 60); do test -f f4.txt && break; sleep 0.5; done
    ok "the daemon pass moved main" test -f f4.txt
    stop_daemon $dpid
fi

finish
