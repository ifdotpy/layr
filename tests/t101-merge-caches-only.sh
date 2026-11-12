#!/bin/bash
# merge --overlays-only (docs/cache-overlays.md): a line takes the caches of another line without
# its files or a merge commit. A protected line takes them only when every path that the source
# changed holds the same content in the target (a squash merge passes). One user, so it runs on
# macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
rm -rf target; mkdir -p target/debug; echo bin0 > target/debug/x
l save --overlays -q

l line new work main >/dev/null
cd $P/work
echo 'first' > code.txt
l save -a -m "change code" -q
rm -rf target; mkdir -p target/debug; echo bin1 > target/debug/x
l save --overlays -q

# 1. Unprotected target: the cache moves; the head, the files and the merge commit do not.
cd $P/main
before=$(l rev-parse head)
out=$(l merge work --overlays-only 2>&1)
has "the merge reports the taken cache" "Merged cache target" "$out"
eq "main head did not move" "$before" "$(l rev-parse head)"
eq "main has the cache of work" bin1 "$(cat target/debug/x)"
no "main has no file of work" test -e code.txt
ok "fsck passes after the take" l fsck

# 2. Protected target, squash-merged: main holds the same content at the changed path under a new
#    commit id. The take passes.
echo 'first' > code.txt
l add code.txt
l save -m "squash of work" -q
l protect main >/dev/null
ok "a squash-merged source takes its caches" l merge work --overlays-only
eq "the squash-merged main has the cache of work" bin1 "$(cat target/debug/x)"

# 3. Protected target, different content at a changed path: refused, the reason names the path.
cd $P/work
echo 'second' > code.txt
l save -a -m "change code again" -q
rm -rf target; mkdir -p target/debug; echo bin2 > target/debug/x
l save --overlays -q
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
echo 'other' > code.txt
l save -a -m "diverge code" -q
l protect main >/dev/null
out=$(l merge work --overlays-only 2>&1); rc=$?
eq "a protected target with other content refuses" 1 "$rc"
has "and names the changed path" "code.txt" "$out"
eq "the cache of main did not change" bin1 "$(cat target/debug/x)"

# 4. The source head matches the target, but its cache was saved with an older state that differs
#    there: the check reads the state the cache was saved with, so the take is refused.
cd $P/work
echo 'other' > code.txt
l save -a -m "same content as main" -q
cd $P/main
before_cache=$(cat target/debug/x)
out=$(l merge work --overlays-only 2>&1); rc=$?
eq "a cache saved with another state is refused" 1 "$rc"
has "and the refusal names the changed path" "code.txt" "$out"
eq "the cache of main did not change after the refusal" "$before_cache" "$(cat target/debug/x)"

finish
