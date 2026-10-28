#!/bin/bash
# Seconds per commit of `layr git fetch`: a remote with FILES files gets COMMITS commits that
# each touch a few files (edit, add, delete, rename), then one fetch imports them.
# Usage: LAYR=target/release/layr benchmarks/git-import/run.sh [files] [commits] [nested]
# Needs a case-sensitive APFS volume in LAYR_APFS_TEST_ROOT (a temp store is made there).
set -eu
FILES=${1:-3000}; COMMITS=${2:-300}; NESTED=${3:-0}
: "${LAYR:?}"; : "${LAYR_APFS_TEST_ROOT:?}"
T=$(mktemp -d "$LAYR_APFS_TEST_ROOT/layr-bench.XXXXXX"); [ -d "$T" ]
export LAYR_ROOT=$T/store GIT_AUTHOR_NAME=B GIT_AUTHOR_EMAIL=b@example.com GIT_COMMITTER_NAME=B GIT_COMMITTER_EMAIL=b@example.com
export LAYR_AUTHOR_NAME=B LAYR_AUTHOR_EMAIL=b@example.com LAYR_TEST_DIRECT_MAIN=1
unset LAYR_PROJECT LAYR_LINE LAYR_SOCKET
R=$T/remote.git; U=$T/up
git init -q --bare -b main "$R"; git clone -q "$R" "$U" 2>/dev/null; cd "$U"
for i in $(seq 1 "$FILES"); do d=d$((i % 60))/s$((i % 7)); mkdir -p "$d"; echo "file $i" > "$d/f$i.txt"; done
if [ "$NESTED" = 1 ]; then git init -q sub; echo x > sub/a; (cd sub && git add -A && git commit -qm sub); mkdir -p .layr-nested; fi
git add -A; git commit -qm base; git push -q origin main
"$LAYR" git clone "$R" bench -q >/dev/null
(cd "$LAYR_ROOT/projects/bench/lines/main" && "$LAYR" protect main --remove >/dev/null)
cd "$U"
for i in $(seq 1 "$COMMITS"); do
  f1=$(ls d$((i % 60))/s$((i % 7)) | head -1); echo "c $i" >> "d$((i % 60))/s$((i % 7))/$f1"
  echo "new $i" > "d1/s1/new$i.txt"; git add -A
  if [ $((i % 10)) = 0 ]; then git mv "d1/s1/new$((i-1)).txt" "d1/s1/renamed$i.txt"; fi
  if [ $((i % 7)) = 0 ]; then git rm -q "d1/s1/new$((i-3)).txt" 2>/dev/null || true; fi
  if [ $((i % 25)) = 0 ]; then
    git checkout -q -b side$i HEAD~1 2>/dev/null && echo "s $i" > "side$i.txt" && git add -A && git commit -qm "side $i" \
      && git checkout -q main && git merge -q --no-ff -m "merge $i" side$i 2>/dev/null || { git merge --abort 2>/dev/null; git checkout -q main; git commit -qm "c $i" --allow-empty; }
  else git commit -qm "c $i"; fi
done
git push -q origin main
N=$(git rev-list --count main); N=$((N - 1 - 0))
cd "$LAYR_ROOT/projects/bench/lines/main"
s=$(date +%s)
"$LAYR" git fetch origin main >/dev/null 2>"$T/fetch.err"
e=$(date +%s)
n=$(git -C "$U" rev-list --count main)
echo "files=$FILES commits_in_remote=$n fetch_seconds=$((e - s)) per_commit=$(echo "scale=3; ($e - $s) / ($n - 1)" | bc)"
echo "store: $T"
