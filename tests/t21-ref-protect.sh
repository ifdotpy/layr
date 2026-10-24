#!/bin/bash
# Ref protection uses exact-state evidence and the existing users and roles.
. "$(dirname "$0")/lib.sh"
chmod 755 "$T" "$LAYR_ROOT"
for user in layr-admin layr-u1 layr-u2; do id "$user" >/dev/null 2>&1 || useradd -m "$user"; done
getent group layr-reviewers >/dev/null || groupadd layr-reviewers
usermod -aG layr-reviewers layr-admin
usermod -aG layr-reviewers layr-u1
export LAYR_SOCKET=$T/layr.sock
cp "$LAYR" "$T/layr"; chmod 755 "$T/layr"
"$T/layr" daemon --socket "$LAYR_SOCKET" --tick 60 > "$T/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
as() { local user=$1; shift; runuser -u "$user" -- env LAYR_ROOT="$LAYR_ROOT" LAYR_SOCKET="$LAYR_SOCKET" PATH=/usr/bin:/bin "$@"; }
L=$T/layr
sample "$T/src"
l init p --from "$T/src" --owner layr-admin -q >/dev/null
l --project p access grant layr-u1 writer
l --project p access grant layr-u2 maintainer
l --project p line new topic main --owner layr-u1 -q
P=$LAYR_ROOT/projects/p/lines
cd /tmp
as layr-u1 sh -c "cd '$P/topic' && echo topic > topic.txt && '$L' add topic.txt && '$L' save -qm topic"
BASE=$(l --project p --line main rev-parse head)
SUBJECT=$(l --project p --line topic rev-parse head)
l --project p ref set env/prod "$BASE" >/dev/null
l --project p protect env/prod --update maintainer --approvals 1 --approvers @layr-reviewers --check smoke
has 'a writer lacks the update role' 'update role maintainer' "$(as layr-u1 "$L" --project p ref set env/prod "$SUBJECT" --expect "$BASE" 2>&1)"
has 'a protected ref requires expect' 'add --expect' "$(l --project p ref set env/prod "$SUBJECT" 2>&1)"
has 'the exact subject needs approval' 'it has 0' "$(l --project p ref set env/prod "$SUBJECT" --expect "$BASE" 2>&1)"
as layr-u1 "$L" --project p approve -q "$SUBJECT"
as layr-u2 "$L" --project p approve -q "$SUBJECT"
has 'authors and non-members do not count' 'it has 0' "$(l --project p ref set env/prod "$SUBJECT" --expect "$BASE" 2>&1)"
as layr-admin "$L" --project p approve -q "$SUBJECT"
has 'an undefined check is named' "check 'smoke'" "$(l --project p ref set env/prod "$SUBJECT" --expect "$BASE" 2>&1)"
CMD="echo checked >> $T/check-count"
l --project p config check.smoke "$CMD"
has 'a missing check is named' "passing check 'smoke'" "$(l --project p ref set env/prod "$SUBJECT" --expect "$BASE" 2>&1)"
no 'ref set never runs a check command' test -e "$T/check-count"
l --project p try -q "$BASE" -- sh -c "$CMD"
no 'a check on another state does not count' l --project p ref set env/prod "$SUBJECT" --expect "$BASE"
l --project p try -q "$SUBJECT" -- true
no 'a check with another command does not count' l --project p ref set env/prod "$SUBJECT" --expect "$BASE"
l --project p config check.smoke '"false"'
no 'a failed matching check is recorded' l --project p try -q "$SUBJECT" -- sh -c false
no 'a failed matching check does not count' l --project p ref set env/prod "$SUBJECT" --expect "$BASE"
l --project p config check.smoke "$CMD"
l --project p try -q "$SUBJECT" -- sh -c "$CMD"
ok 'all required evidence permits an update' as layr-admin "$L" --project p ref set env/prod "$SUBJECT" --expect "$BASE"
COUNT=$(wc -l < "$T/check-count" | xargs)
ok 'a protected delete uses evidence of its current subject' as layr-admin "$L" --project p ref delete env/prod --expect "$SUBJECT"
eq 'ref operations execute no check commands' "$COUNT" "$(wc -l < "$T/check-count" | xargs)"
# A rule can allow a writer; the old maintainer floor must not override it.
l --project p protect 'release/*' --update writer --no-expect
ok 'a wildcard ref rule permits its update role' as layr-u1 "$L" --project p ref set release/test "$SUBJECT"
for flag in --direct --no-direct; do no "refs refuse $flag" l --project p protect env/prod "$flag"; done
no 'refs refuse rewrite rules' l --project p protect env/prod --rewrite admin
no 'refs refuse path rules' l --project p protect env/prod --path src=admin
# Queue merges retain the existing approval rule on the incoming exact state.
l --project p protect main --approvals 1 --approvers @layr-reviewers
as layr-u1 sh -c "cd '$P/topic' && echo next >> topic.txt && '$L' save -qam next"
NEXT=$(l --project p --line topic rev-parse head)
Q=$(as layr-u1 "$L" --project p --line topic queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
no 'a queue merge needs approval' as layr-admin "$L" --project p queue run --into main
has 'the failed entry explains approval' 'approval(s)' "$(l --project p queue list)"
eq 'the unapproved target stays' "$BASE" "$(l --project p --line main rev-parse head)"
as layr-admin "$L" --project p approve -q "$NEXT"
as layr-admin "$L" --project p queue add "$NEXT" >/dev/null
ok 'the approved exact state merges from the queue' as layr-admin "$L" --project p queue run --into main
eq 'the approved state becomes the head' "$NEXT" "$(l --project p --line main rev-parse head)"
# Automatic checks use the target owner, not the caller who submitted the entry.
as layr-u1 sh -c "cd '$P/topic' && echo automatic >> topic.txt && '$L' save -qam automatic"
AUTOMATIC=$(l --project p --line topic rev-parse head)
as layr-admin "$L" --project p approve -q "$AUTOMATIC"
: > "$T/identity"; chmod 666 "$T/identity"
l --project p protect main --check identity
l --project p config check.identity "id -un > $T/identity"
l --project p config queue.run main
AUTO=$(as layr-u1 "$L" --project p --line topic queue add head --json)
eq 'automatic passes use the target owner' layr-admin "$(cat "$T/identity")"
eq 'automatic completion respects normal policy' done "$(printf '%s' "$AUTO" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
has 'records verify after ref and queue policy checks'  'signatures ok' "$(l --project p fsck)"
finish
