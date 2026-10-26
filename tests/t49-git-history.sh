#!/bin/bash
# Exact Git DAG and object bytes survive encrypted store push and a fresh-machine clone.
. "$(dirname "$0")/lib.sh"
set -e
S=$T/source
mkdir -p "$S"
git -C "$S" init -q -b main
cd "$S"
printf 'one\n' > text
printf '\000\377binary\n' > binary
printf 'tracked-ignored\n' > ignored.log
printf '*.log\n' > .gitignore
printf '#!/bin/sh\nexit 0\n' > executable
chmod +x executable
ln -s text link
git add -f .
git update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,module
git commit -qm first
printf 'two\n' >> text
git commit -qam second
git switch -qc side
mkdir -p nested
printf 'shared\n' > nested/shared
git add nested
git commit -qm side
# Preserve arbitrary headers, distinct identities, timezones, signatures and non-UTF-8 messages.
python3 - <<'PY'
import subprocess
raw = subprocess.check_output(['git', 'cat-file', 'commit', 'HEAD'])
head, message = raw.split(b'\n\n', 1)
raw = head + b'\nencoding ISO-8859-1\ngpgsig -----BEGIN PGP SIGNATURE-----\n example\n -----END PGP SIGNATURE-----\n\nside caf\xe9\n'
oid = subprocess.check_output(['git', 'hash-object', '-w', '-t', 'commit', '--stdin'], input=raw).strip()
subprocess.run(['git', 'update-ref', 'refs/heads/side', oid], check=True)
PY
git switch -q main
mkdir -p nested
printf 'shared\n' > nested/shared
git add nested
git commit -qm third
git merge --no-ff side -qm merge
original_tip=$(git rev-parse main)
original_ids=$(git rev-list main | sort)
original_tree=$(git rev-parse 'main^{tree}')

eq 'fixture contains five commits' 5 "$(git rev-list --count main)"
l git clone "$S" history -q >/dev/null
cd "$LAYR_ROOT/projects/history/lines/main"
eq 'import maps all five commits' 5 "$(l log --format=%s | wc -l | tr -d ' ')"
eq 'import keeps original Git tip' "$original_tip" "$(git rev-parse HEAD)"
records=$(l op log --all | wc -l)
l git fetch -q
eq 'reimport creates no duplicate states' "$records" "$(l op log --all | wc -l)"
l remote add archive "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
l push archive -q
original_root=$LAYR_ROOT
new_fs "$T/replica" 8G
export LAYR_ROOT=$T/replica LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
l clone "file://$T/archive" history -q
cd "$LAYR_ROOT/projects/history/lines/main"
eq 'fresh clone restores identical commit IDs' "$original_ids" "$(git rev-list HEAD | sort)"
eq 'fresh clone restores exact original tree' "$original_tree" "$(git rev-parse 'HEAD^{tree}')"
eq 'tracked ignored file survives' tracked-ignored "$(cat ignored.log)"
eq 'symlink survives' text "$(readlink link)"
ok 'executable survives' test -x executable
ok 'restored Git objects are connected' git fsck --no-reflogs
R=$T/mirror.git
git init --bare -q -b main "$R"
l git remote add github "$R"
l git push -q github main
eq 'empty mirror receives all original IDs' "$original_ids" "$(git -C "$R" rev-list main | sort)"
eq 'mirror tip is original commit' "$original_tip" "$(git -C "$R" rev-parse main)"
printf 'new\n' > after
l add after
l save -qm after
l git push -q github main
new_tip=$(git -C "$R" rev-parse main)
eq 'new commit continues original history' "$original_tip" "$(git -C "$R" rev-parse main^)"
eq 'new commit adds one to count' 6 "$(git -C "$R" rev-list --count main)"
l reset --hard -q head~1
out=$(l git push github main 2>&1) && rc=0 || rc=$?
eq 'non-fast-forward returns failure' 128 "$rc"
has 'refusal names remote ID' "$new_tip" "$out"
has 'refusal names proposed ID' "$original_tip" "$out"
eq 'refusal keeps mirror unchanged' "$new_tip" "$(git -C "$R" rev-parse main)"
l git push --force -q github main
eq 'explicit force permits rewind' "$original_tip" "$(git -C "$R" rev-parse main)"
finish
