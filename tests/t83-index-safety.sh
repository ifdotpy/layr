#!/bin/bash
# The index (stage) keeps staged content:
# 1. a reading command never deletes an index that looks stale (another command may have
#    written it for a newer head), 2. `rm` refuses a staged version that differs from both the
#    head and the file, and `line delete` keeps the index so `undo` restores it, 3. adding a
#    file replaces its index copy without touching other links to the old inode.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p
cd $P/lines/main
echo shared > a && ln a b && echo other > c
l add a b c && l save -m links -q

# 1. A reading command removes no stage file.
echo staged > notes.txt
l add notes.txt
stage=$(ls $P/meta/stage | grep -v '\.base$' | head -1)
base=$(cat $P/meta/stage/$stage.base)
ok "the index exists" test -e $P/meta/stage/$stage
printf 'not-the-head' > $P/meta/stage/$stage.base
l status >/dev/null 2>&1
l status --short >/dev/null 2>&1
l diff >/dev/null 2>&1
l diff --cached >/dev/null 2>&1
ok "status and diff keep a stale looking index" test -e $P/meta/stage/$stage
ok "status and diff keep its base file" test -e $P/meta/stage/$stage.base
printf '%s' "$base" > $P/meta/stage/$stage.base
has "the staged version is still there" "M  notes.txt" "$(l status --short 2>&1)"
l restore --staged notes.txt >/dev/null 2>&1 || l reset -q -- notes.txt >/dev/null 2>&1

# 2a. rm of a staged version that differs from head and file.
cd $P/lines/main
echo staged2 > notes.txt; l add notes.txt; echo worked > notes.txt
no "rm refuses a staged version found nowhere else" l rm notes.txt
out=$(l rm notes.txt 2>&1)
has "rm names the file" "notes.txt" "$out"
has "the file stays in the working folder" worked "$(cat notes.txt)"
has "the staged version is kept" "+staged2" "$(l diff --cached 2>&1)"
no "rm --cached refuses it too" l rm --cached notes.txt
ok "rm -f removes it" l rm -f notes.txt
# staged equal to the file, and staged equal to the head, remove without -f.
echo again > notes.txt; l add notes.txt
ok "rm of staged equal to the file" l rm notes.txt
l reset --hard -q >/dev/null 2>&1
ok "rm of an unchanged file" l rm c

# 2b. line delete keeps the index; undo brings it back.
cd $P/lines/main
l reset --hard -q >/dev/null 2>&1
l line new w -q >/dev/null 2>&1
cd $P/lines/w
echo staged3 > notes.txt; l add notes.txt; echo worked3 > notes.txt
cd $P/lines/main
l line delete -D w >/dev/null 2>&1
ok "the line is gone" test ! -e $P/lines/w
l undo >/dev/null 2>&1
ok "undo makes the line again" test -e $P/lines/w
cd $P/lines/w
eq "the working file is back" worked3 "$(cat notes.txt)"
has "the staged version is back" "+staged3" "$(l diff --cached 2>&1)"
cd $P/lines/main

# 3. add replaces its own index copy only.
cd $P/lines/main
cp a a.new && echo new >> a.new && mv a.new a
l add a
st=$(l status --short 2>&1)
has "a is staged" "M  a" "$st"
eq "b is not staged" 0 "$(echo "$st" | grep -c '^M  b')"
eq "b keeps its content in the index" shared "$(l show :b 2>/dev/null || l show HEAD:b)"
has "b is unchanged in the working folder" shared "$(cat b)"
finish
