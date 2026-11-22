#!/bin/bash
# E1: the files a build uses, seen by the server (trace), and a check of the prediction.
. ~/lfx/lab/lab.sh
SRC=/home/layrfs-src/layr
vnew tr layr-base
P=$(M tr); cd $P
t0=$(now); cargo build --offline -q 2>/dev/null; t1=$(now); rec e1 "cargo build (full)" $(ms $t0 $t1)
nb=(); for i in 1 2 3; do t0=$(now); cargo build --offline -q 2>/dev/null; t1=$(now); nb+=($(ms $t0 $t1)); done
rec e1 "no-op cargo build, no trace (median of 3)" "$(printf '%s\n' "${nb[@]}" | median)"
t0=$(now); out=$(vctl tr trace start); t1=$(now); rec e1 "trace start" $(ms $t0 $t1) "\"out\":\"$out\""
t0=$(now); cargo build --offline -q 2>/dev/null; t1=$(now); rec e1 "no-op cargo build, traced" $(ms $t0 $t1)
vctl tr trace stop > ~/lfx/lab/trace-noop.txt
nb=(); for i in 1 2 3; do t0=$(now); cargo build --offline -q 2>/dev/null; t1=$(now); nb+=($(ms $t0 $t1)); done
rec e1 "no-op cargo build after trace (median of 3)" "$(printf '%s\n' "${nb[@]}" | median)"
for t in R S L M W; do echo "$t $(grep -c "^$t " ~/lfx/lab/trace-noop.txt)"; done | tr '\n' ' '; echo
touch src/main.rs; vctl tr trace start >/dev/null
t0=$(now); cargo build --offline -q 2>/dev/null; t1=$(now); rec e1 "cargo build after touch main.rs, traced" $(ms $t0 $t1)
vctl tr trace stop > ~/lfx/lab/trace-touch.txt
for t in R S L M W; do echo "$t $(grep -c "^$t " ~/lfx/lab/trace-touch.txt)"; done | tr '\n' ' '; echo
# Prediction: a file outside the traced set cannot change the build.
grep '^S ' ~/lfx/lab/trace-noop.txt | cut -c3- | sort -u > ~/lfx/lab/inset.txt
grep -E '\.(rs|toml|md|sh|txt|yml|yaml)$' $LLIST | grep -v '^\.git/' | sort > ~/lfx/lab/cand.txt
comm -12 ~/lfx/lab/cand.txt ~/lfx/lab/inset.txt | shuf -n ${K:-10} --random-source=<(yes) > ~/lfx/lab/pick-in.txt
comm -23 ~/lfx/lab/cand.txt ~/lfx/lab/inset.txt | shuf -n ${K:-10} --random-source=<(yes) > ~/lfx/lab/pick-out.txt
echo "set: $(wc -l < ~/lfx/lab/inset.txt) paths; candidates in set: $(comm -12 ~/lfx/lab/cand.txt ~/lfx/lab/inset.txt | wc -l), outside: $(comm -23 ~/lfx/lab/cand.txt ~/lfx/lab/inset.txt | wc -l)"
cmt() { case $1 in *.rs) echo '// lab';; *.md|*.txt) echo 'lab';; *) echo '# lab';; esac; }
check() { # check <list> <predicted>
  local f rebuilt
  while read -r f; do
    cmt "$f" >> "$P/$f"
    rebuilt=$(cargo build --offline 2>&1 | grep -c 'Compiling' || true)
    cp "$SRC/$f" "$P/$f"; cargo build --offline -q 2>/dev/null || true
    echo "{\"exp\":\"e1\",\"file\":\"$f\",\"predicted\":\"$2\",\"compiled\":$rebuilt}" >> $R
    printf 'e1 %-50s predicted %-8s compiled %s\n' "$f" "$2" "$rebuilt"
  done < "$1"
}
check ~/lfx/lab/pick-out.txt no
check ~/lfx/lab/pick-in.txt maybe
cd /; vdel tr
