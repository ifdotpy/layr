#!/bin/bash
# A folder that `.layrattributes` marks regenerable is left out of the walks of status, diff and
# add at once, and becomes a cache overlay on the next save. A regenerable folder whose files
# the head versions is no cache: it is walked as before, and a named save says so.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf 'big/\nbig2/\n' >> $T/src/.gitignore
printf 'big regenerable\nbig2 regenerable\nvend regenerable\n' > $T/src/.layrattributes
mkdir -p $T/src/vend && echo tracked > $T/src/vend/keep.txt
l init r --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/r/lines
cd $P/main || exit 1
# vend/ is ignored now, but the head holds its file.
echo 'vend/' >> .gitignore
l save -a -qm ignore-vend
python3 - <<'PY'
import os
for d in range(50):
    os.makedirs(f"big/d{d}", exist_ok=True)
    for f in range(100):
        with open(f"big/d{d}/f{f}", "w") as h:
            h.write("x")
PY
eq "big/ has 5000 files" 5000 "$(find big -type f | wc -l | tr -d ' ')"
echo edit >> notes.txt

# Entries that the working folder scans of a command visited (APFS reports them).
visited() { awk '/APFS bulk scan visited/ { n += $8; seen = 1 } END { print seen ? n : 999999999 }' "$1"; }
below() { # below <limit> <description> <trace file>: the scans saw few entries
    if [ "$(uname -s)" = Darwin ]; then
        ok "$2 (visited $(visited "$3"))" test "$(visited "$3")" -lt "$1"
    fi
}

LAYR_TRACE=1 l status -s > $T/status.out 2> $T/status.trace
eq "status shows the edit only" " M notes.txt" "$(grep -v '^??' $T/status.out)"
below 1000 "status visits no entry under big/" $T/status.trace
LAYR_TRACE=1 l diff > $T/diff.out 2> $T/diff.trace
has "diff shows the edit" "notes.txt" "$(cat $T/diff.out)"
below 1000 "diff visits no entry under big/" $T/diff.trace
echo new > new.txt
ok "add of a file next to big/" l add new.txt
no "big/ is no overlay before a save" sh -c "$LAYR overlay list | grep -q big"

# The next save (automatic) makes the folder a cache overlay; the state has none of its files.
LAYR_TRACE=1 l save -q 2> $T/save.trace
has "overlay list shows the cache overlay of big/" "big" "$(l overlay list)"
has "it is a cache" "cache" "$(l overlay list | grep big)"
eq "the state holds no file of big/" "" "$(l show head@{0}:big | tail -n +3)"
eq "the files are still in the folder" 5000 "$(find big -type f | wc -l | tr -d ' ')"
LAYR_TRACE=1 l status -s > $T/status2.out 2> $T/status2.trace
below 1000 "status after the save visits no entry under big/" $T/status2.trace

# A named save also leaves big/ out, and says which regenerable folder is versioned.
mkdir big2 && seq 1 200 > big2/list
echo two >> notes.txt
out=$(LAYR_TRACE=1 l save -a -m "named" 2> $T/named.trace)
out="$out$(cat $T/named.trace)"
has "a named save says the versioned folder is no cache" "vend: not a cache" "$out"
no "the versioned folder is no overlay" sh -c "$LAYR overlay list | grep -q vend"
eq "its file stays in the state" tracked "$(l show head:vend/keep.txt)"
has "a named save makes big2/ a cache overlay too" "big2" "$(l overlay list)"
below 1000 "a named save visits no entry under big/" $T/named.trace
eq "the named state holds no file of big/" "" "$(l show head:big | tail -n +3)"
finish
