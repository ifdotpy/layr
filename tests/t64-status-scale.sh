#!/bin/bash
# `layr status` on a line like a big project's: a cache overlay folder of 100k small files, a
# nested git repository, a long history and other live lines. It must finish in seconds and
# must not hold the project lock while it compares overlays and other lines.
. "$(dirname "$0")/lib.sh"

now_s() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "import time; print(int(time.time() - $1))"; }

sample $T/src
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
# A long history first (saves are fast while the tree is small).
for i in $(seq 1 60); do echo "n$i" > notes-$((i % 5)).txt; l save -a -m "n$i" -q; done
python3 - <<'PY'
import os
for d in range(100):
    os.makedirs(f"target/d{d}", exist_ok=True)
    for f in range(1000):
        with open(f"target/d{d}/f{f}", "w") as h:
            h.write("x")
PY
mkdir vendor && git -C vendor init -q && echo v > vendor/a.txt
l save --caches -m "cache and nested repo" -q
for n in a b c d; do
    l line new $n main >/dev/null
    (cd $P/$n && echo "$n" > $n.txt && l save -a -m "$n" -q)
done
echo edit >> notes.txt

# 1. Status finishes in seconds.
t0=$(now_s)
out=$(l status 2>&1)
secs=$(since $t0)
has "status shows the edit" "notes.txt" "$out"
eq "status of the cache folder files is not listed" 0 "$(echo "$out" | grep -c 'target/')"
ok "status of the big line takes at most 15 seconds ($secs s)" test "$secs" -le 15

# 2. Another command takes the project lock while status compares other lines.
LAYR_TRACE=1 l status >/dev/null 2>$T/trace.txt &
spid=$!
waited=0
while kill -0 $spid 2>/dev/null; do
    t1=$(now_s)
    (cd $P/a && l save -a -m poll -q --allow-empty 2>/dev/null || true)
    w=$(since $t1)
    [ "$w" -gt "$waited" ] && waited=$w
done
wait $spid
ok "a command waits at most 10 seconds for the lock during status ($waited s)" test "$waited" -le 10
finish
