#!/bin/bash
# Bisect, secret refusals, and value declassification.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
GOOD=$(l rev-parse HEAD)
CMD='test ! -f broken'
l try -q HEAD -- sh -c "$CMD"
for i in 1 2 3; do echo "$i" >> notes.txt; l commit -qam "good $i"; done
echo bad > broken
l add broken
l commit -qm 'first bad'
BAD=$(l rev-parse HEAD)
for i in 4 5 6; do echo "$i" >> notes.txt; l commit -qam "bad $i"; done
TIP=$(l rev-parse HEAD)
RESULT=$(l bisect run --json --good "$GOOD" --bad HEAD -- sh -c "$CMD")
eq 'bisect finds the first bad commit' "$BAD" "$(l rev-parse "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')")"
eq 'bisect does not move the line' "$TIP" "$(l rev-parse HEAD)"
RESULT=$(l bisect run --json --good "$GOOD" --bad HEAD -- sh -c "$CMD")
eq 'bisect reuses every check on repeat' True "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(all(s["reused"] for s in json.load(sys.stdin)["steps"]))')"
no 'bisect refuses an invalid good bound' l bisect run --good HEAD --bad "$GOOD" -- sh -c "$CMD"
no 'bisect refuses a passing bad bound' l bisect run --good "$GOOD" --bad "$GOOD" -- true
# Ref positions can skip the commit that introduced the fault.
l ref set env/prod "$GOOD" >/dev/null
l ref set env/prod "$BAD" >/dev/null
l ref set env/prod HEAD >/dev/null
RESULT=$(l bisect run --ref-log env/prod --json -- sh -c "$CMD")
eq 'ref bisect uses its positions and defaults' "$BAD" "$(l rev-parse "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')")"
no 'ref bisect refuses an unknown position' l bisect run --ref-log env/prod --good HEAD~1 --bad HEAD -- sh -c "$CMD"
# Argument boundaries are part of command identity, despite equal display text.
l try -q "$GOOD" -- sh -c exit 1
no 'bisect does not reuse a different argument vector' l bisect run --good "$GOOD" --bad "$BAD" -- sh -c 'exit 1'
RESULT=$(l bisect run --json --good "$GOOD" --bad "$BAD" -- sh -c 'echo check-output; test ! -f broken')
ok 'test output does not corrupt JSON' python3 -c 'import json,sys; json.loads(sys.argv[1])' "$RESULT"
l attest revoke "$GOOD" >/dev/null
RESULT=$(l bisect run --json --good "$GOOD" --bad "$BAD" -- sh -c "$CMD")
eq 'bisect does not reuse a revoked check' False "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["steps"][0]["reused"])')"
# Auto states retain the exact tool-call origin digest.
l branch -q autos "$GOOD"
cd ../autos
echo good > auto.txt
l save -q
echo bad > broken
l save -q
AUTO=$(l rev-parse 'HEAD@{0}')
printf 'tool-call-47' > "$T/origin"
l attest origin "$AUTO" --payload "$T/origin" >/dev/null
echo later >> auto.txt
l save -q
RESULT=$(l bisect run --auto --good HEAD --json -- sh -c "$CMD")
eq 'auto bisect finds the first failed save' "$AUTO" "$(l rev-parse "$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')")"
has 'auto result includes the origin digest' "$(sha256sum "$T/origin" | cut -d' ' -f1)" "$RESULT"
eq 'auto bisect preserves the working content' bad "$(cat broken)"
# A configured source is not shared and remains outside the commit.
cd ../main
printf '.env\n' >> .gitignore
l commit -qam ignore
l config secrets.paths '[".env"]'
SECRET=sk_live_1234567890abcdef
printf 'TOKEN=%s\n' "$SECRET" > .env
printf 'normal\npassword=%s\n' "$SECRET" > leak.txt
l add leak.txt
HEAD_BEFORE=$(l rev-parse HEAD)
OUT=$(l commit -qm leak 2>&1); RC=$?
eq 'secret commit fails' 128 "$RC"
has 'refusal gives path and line' 'leak.txt:2' "$OUT"
no 'refusal never gives the value' sh -c 'case "$1" in *"$2"*) exit 0;; *) exit 1;; esac' sh "$OUT" "$SECRET"
eq 'refusal leaves the head' "$HEAD_BEFORE" "$(l rev-parse HEAD)"
ID=$(printf '%s\n' "$OUT" | sed -n 's/.*declassify id \([^;]*\);.*/\1/p')
ok 'admin declassifies the value' l approve --declassify "$ID" HEAD
ok 'the next commit accepts that value and path' l commit -qm declassified
OUT=$(l export HEAD "$T/export-ok" 2>&1); RC=$?
eq 'export accepts the declassified state' 0 "$RC"
if [ "$RC" -ne 0 ]; then printf '%s\n' "$OUT"; fi
printf 'password=%s\n' "$SECRET" > other.txt
l add other.txt
no 'the approval does not cover another path' l commit -qm other
l restore -SW -- other.txt
l approve --withdraw "$HEAD_BEFORE" -q
no 'withdrawal restores the export refusal' l export HEAD "$T/export-no"
# A folder export sends the selected tree, so removed values do not block it.
l branch -q scrubbed
cd ../scrubbed
l rm leak.txt >/dev/null
l commit -qm scrubbed
ok 'a folder export accepts a tree after secret removal' l export HEAD "$T/export-scrubbed"
cd ../main
# Existing states must also be checked before merge and push.
l branch -q destination "$HEAD_BEFORE"
cd ../destination
BEFORE=$(l rev-parse HEAD)
no 'fast-forward merge refuses the secret' l merge main
eq 'merge refusal keeps the target head' "$BEFORE" "$(l rev-parse HEAD)"
cd ../main
mkdir "$T/git-remote"
git init --bare -q "$T/git-remote"
l git remote add origin "$T/git-remote"
no 'Git push refuses the secret' l git push origin HEAD:main
# The private index contains hashes only.
META="$LAYR_ROOT/projects/p/meta"
eq 'the key is private' 600 "$(stat -c %a "$META/secrets.key")"
no 'the index never contains the source value' grep -R -F "$SECRET" "$META/secrets-index-0.json"
# Changed source values cause another refusal without a manual rebuild.
printf 'TOKEN=sk_changed_0987654321\n' > .env
printf 'sk_changed_0987654321\n' > fresh.txt
l add fresh.txt
no 'source changes rebuild the index' l commit -qm fresh
# Private overlay values are sources even without secrets.paths.
l branch -q private "$GOOD"
cd ../private
l config secrets.paths '[]'
l overlay create tokens -q
printf 'TOKEN=sk_overlay_1234567890\n' > token.env
l overlay move tokens token.env >/dev/null
ok 'private overlay commit accepts the secret' l commit -qm private
printf 'sk_overlay_1234567890\n' > public.txt
l add public.txt
no 'a private overlay value cannot enter the shared set' l commit -qm leak
# Large files use a stream check, including tokens across a 1 MiB boundary.
l branch -q large "$GOOD"
cd ../large
l config secrets.paths '[".env"]'
printf 'TOKEN=sk_boundary_1234567890\n' > .env
python3 - <<'LARGE'
with open('clean.bin', 'wb') as f:
    f.truncate(65 * 1024 * 1024)
LARGE
l add clean.bin
ok 'a large file without a secret can commit' l commit -qm large-clean
python3 - <<'LARGE'
with open('large.bin', 'wb') as f:
    f.truncate(65 * 1024 * 1024)
    f.seek(1024 * 1024 - 5)
    f.write(b'sk_boundary_1234567890')
LARGE
l add large.bin
OUT=$(l commit -qm large-leak 2>&1)
has 'a large file finds a secret across a chunk boundary' 'large.bin:byte 1048571' "$OUT"
no 'the large-file refusal hides the value' sh -c 'case "$1" in *sk_boundary_1234567890*) exit 0;; *) exit 1;; esac' sh "$OUT"
# The service enforces the admin boundary for declassification.
chmod 755 "$T" "$LAYR_ROOT"
id layr-guard >/dev/null 2>&1 || useradd -m layr-guard
l access grant layr-guard writer
l branch --owner layr-guard -q writer "$GOOD"
P="$LAYR_ROOT/projects/p/lines/writer"
printf 'TOKEN=sk_admin_0123456789\n' > "$P/.env"
printf 'sk_admin_0123456789\n' > "$P/public.txt"
chown layr-guard:layr-guard "$P/.env" "$P/public.txt"
cp "$LAYR" "$T/layr"
chmod 755 "$T/layr"
export LAYR_SOCKET="$T/layr.sock"
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 3600 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
as_writer() { (cd /tmp && runuser -u layr-guard -- env LAYR_SOCKET="$LAYR_SOCKET" LAYR_PROJECT=p LAYR_LINE=writer PATH=/usr/bin:/bin "$T/layr" "$@"); }
ok 'the writer stages a file through the service' as_writer add public.txt
OUT=$(as_writer commit -qm leak 2>&1)
has 'the service checks the writer sources' 'public.txt:1' "$OUT"
ID=$(printf '%s\n' "$OUT" | sed -n 's/.*declassify id \([^;]*\);.*/\1/p')
no 'a writer cannot declassify a value' as_writer approve --declassify "$ID" HEAD
has 'the refusal names the required role' 'admin role' "$(as_writer approve --declassify "$ID" HEAD 2>&1)"
finish
