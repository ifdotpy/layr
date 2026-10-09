#!/bin/bash
# Regressions from review findings (2026-10-09): links in the .git layer, backup manifest paths,
# repeated backups, an incomplete log line, an index failure after the log write.
. "$(dirname "$0")/lib.sh"

R=$T/r.git; git init -q --bare -b main $R; git clone -q $R $T/up 2>/dev/null
cd $T/up && echo a > a.txt && git add a.txt && git commit -qm a && git push -q origin main
l clone $R p -q >/dev/null
W=$LAYR_ROOT/projects/p/lines/main
cd $W

# 1. The .git layer belongs to the line owner: links in it must not redirect root writes.
echo victim-head > $T/victim1; echo victim-index > $T/victim2
rm .git/HEAD .git/index; ln -s $T/victim1 .git/HEAD; ln -s $T/victim2 .git/index
echo b > b.txt; l add b.txt; l commit -qm b; l push -q origin t1
eq "a link at .git/HEAD does not redirect the write" "victim-head" "$(cat $T/victim1)"
eq "a link at .git/index does not redirect the write" "victim-index" "$(cat $T/victim2)"
ok "HEAD is a regular file again" test -f .git/HEAD -a ! -L .git/HEAD
ok "git works in the line" git log --oneline -1

# 2. Manifest paths stay inside the backup folder.
l backup -q $T/bk
chain=$(ls -d $T/bk/*/chain-0001)
echo precious > $T/precious
cp $chain/manifest.json $T/manifest.bak
python3 - "$chain/manifest.json" "$T/precious" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
for i in m["items"]:
    if i["kind"] == "logs":
        i["file"] = sys.argv[2]
json.dump(m, open(sys.argv[1], "w"))
PY
eq "a manifest with an absolute path is refused" "128" "$(l backup -q $T/bk >/dev/null 2>&1; echo $?)"
eq "and the outside file stays" "precious" "$(cat $T/precious)"
has "verify reports the bad name" "not a plain file name" "$(l backup verify $T/bk 2>&1)"
sed -i 's|"file": *"[^"]*logs[^"]*"|"file":"../../precious"|' $chain/manifest.json 2>/dev/null
cp $T/manifest.bak $chain/manifest.json

# 3. Repeated backups without changes keep a complete chain.
l backup -q $T/bk; l backup -q $T/bk; l backup -q $T/bk
eq "three backups in a row verify" "ok" "$(l backup verify $T/bk | awk '{print $NF}')"

# 4. An incomplete last log line (a crash during a write) does not damage the next record.
log=$LAYR_ROOT/projects/p/meta/log/$(cat $LAYR_ROOT/machine/id).jsonl
printf '{"r":{"id":"partial' >> $log
echo c > c.txt; l add c.txt; l commit -qm c
eq "the next commit works" "c" "$(l log -1 --format=%s)"
l reindex >/dev/null
eq "the log reads after a rebuild" "c" "$(l log -1 --format=%s)"
has "and its signatures verify" "signatures ok" "$(l fsck)"

# 5. An index failure after the log write keeps the snapshot of the recorded state.
echo d > d.txt; l add d.txt
chattr +i $LAYR_ROOT/projects/p/meta/index.sqlite $LAYR_ROOT/projects/p/meta/index.sqlite-wal 2>/dev/null
out=$(l commit -qm d 2>&1); rc=$?
chattr -i $LAYR_ROOT/projects/p/meta/index.sqlite $LAYR_ROOT/projects/p/meta/index.sqlite-wal 2>/dev/null
eq "the commit reports the index failure" "128" "$rc"
has "and says the record is kept" "recorded in the log" "$out"
eq "the next command has the recorded state" "d" "$(l log -1 --format=%s)"
eq "with its snapshot" "d" "$(l show HEAD:d.txt)"
eq "fsck is clean" "0 problem(s)" "$(l fsck | tail -1)"
finish
