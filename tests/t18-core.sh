#!/bin/bash
# Refs, attestations, logical changes, and GC roots.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
S1=$(l rev-parse HEAD)
no 'the layer alias is removed' l layer list
has 'layer names cache' 'layr cache' "$(l layer list 2>&1)"
echo first >> notes.txt
l commit -qam first
S2=$(l rev-parse HEAD)
C=$(l change list --json | python3 -c 'import json,sys; print(next(x["change_id"] for x in json.load(sys.stdin) if x["message"]=="first"))')
ok 'set a ref' l ref set env/prod HEAD
no 'ref set refuses a line' l ref set main HEAD~1
no 'a stale expected position fails' l ref set env/prod HEAD~1 --expect HEAD~1
eq 'failed CAS leaves the position' "$S2" "$(l rev-parse env/prod)"
eq 'failed CAS writes no history' 1 "$(l ref log env/prod --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok 'matching CAS moves the ref' l ref set env/prod HEAD~1 --expect HEAD
eq 'ref current position' "$S1" "$(l rev-parse 'env/prod@{0}')"
eq 'ref previous position' "$S2" "$(l rev-parse 'env/prod@{1}')"
ok 'undo the ref move' l undo
eq 'undo restores the ref' "$S2" "$(l rev-parse env/prod)"
no 'delete CAS fails without change' l ref delete env/prod --expect HEAD~1
ok 'delete CAS succeeds' l ref delete env/prod --expect HEAD
no 'deleted ref does not resolve' l rev-parse env/prod
ok 'undo ref delete' l undo
eq 'undo delete restores the ref' "$S2" "$(l rev-parse env/prod)"
eq 'prefix list' env/prod "$(l ref list env/ --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["name"])')"
for prefix in env release pins holds leases caches derived changes views attestations; do
    no "reserved line prefix $prefix" l branch "$prefix/name"
done
has 'views have an explicit error' 'views are not implemented' "$(l rev-parse views/unknown 2>&1)"
no 'synthetic revisions cannot be mutable refs' l ref set changes/other HEAD
l ref set pins/cas "$S2" >/dev/null
(l ref set pins/cas "$S1" --expect "$S2" >/dev/null 2>&1; echo $? > "$T/cas1") &
(l ref set pins/cas "$S1" --expect "$S2" >/dev/null 2>&1; echo $? > "$T/cas2") &
wait
eq 'concurrent CAS permits one writer' '0 128' "$(sort -n "$T/cas1" "$T/cas2" | xargs)"

printf 'private payload bytes' > "$T/payload"
A=$(l attest origin HEAD --payload "$T/payload" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["attestation_id"])')
eq 'attestation revision resolves its subject' "$S2" "$(l rev-parse attestations/$A)"
eq 'attestation payload digest' "$(sha256sum "$T/payload" | cut -d' ' -f1)" "$(l attestations HEAD --kind origin --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["payload_digest"])')"
no 'invalid kind fails' l attest unknown HEAD
ok 'approve records an attestation' l approve -q HEAD
eq 'approval kind exists' approval "$(l attestations HEAD --kind approval --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["kind"])')"
ok 'withdraw adds a revoke attestation' l approve -q --withdraw HEAD
eq 'the original attestation remains' 1 "$(l attestations HEAD --kind approval --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
eq 'revoke exists' 1 "$(l attestations HEAD --kind revoke --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok 'try writes a passing check attestation' l try -q HEAD -- sh -c 'test -f notes.txt'
no 'try returns the failed command exit code' l try -q HEAD -- sh -c 'exit 7'
eq 'both checks are recorded' 2 "$(l attestations HEAD --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
for old in approvals checks; do
    has "$old names its replacement" 'layr attestations' "$(l "$old" HEAD 2>&1)"
    no "$old fails" l "$old" HEAD
done
echo second >> notes.txt
l commit -qam amended --amend
S3=$(l rev-parse HEAD)
eq 'amend keeps ChangeId' "$S3" "$(l rev-parse changes/$C)"
eq 'first change version stays addressable' "$S2" "$(l rev-parse changes/$C/v1)"
eq 'second change version resolves' "$S3" "$(l rev-parse changes/$C/v2)"
has 'interdiff shows the amended patch' second "$(l change diff "$C" --from v1 --to v2)"
eq 'change show includes attestations for the earlier version' 1 "$(l change show "$C" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["versions"][0]["attestations"])>0 and 1)')"
has 'change log records amend' commit "$(l change log "$C")"
no 'version zero fails' l rev-parse changes/$C/v0
l branch -q copied "$S1"
cd ../copied
l cherry-pick "$S3" >/dev/null
PICK=$(l rev-parse HEAD)
eq 'cherry-pick keeps ChangeId' "$PICK" "$(l rev-parse changes/$C)"
l branch -q base "$S1"
cd ../base
echo base > base.txt
l add base.txt
l commit -qm base
cd ../copied
l rebase base >/dev/null
REB=$(l rev-parse HEAD)
eq 'rebase keeps ChangeId' "$REB" "$(l rev-parse changes/$C)"
eq 'base-only rebase changes do not enter the interdiff' '' "$(l change diff "$C" --from v3 --to v4)"
l branch -q merge-source
cd ../merge-source
echo side > side.txt
l add side.txt
l commit -qm side
cd ../copied
echo own > own.txt
l add own.txt
l commit -qm own
ok 'a merge commit succeeds' l merge --no-ff merge-source
MC=$(l change list --json | python3 -c 'import json,sys; print(next(x["change_id"] for x in json.load(sys.stdin) if x["message"].startswith("Merge ")))')
ok 'a merge gets a new ChangeId' test -n "$MC" -a "$MC" != "$C"
eq 'a merge does not replace the source change identity' "$REB" "$(l rev-parse changes/$C)"
# Auto states without line history roots demonstrate live and expired ref retention.
l config retention.auto 0
l config retention.hours 0
l config undo.days 0
echo detached >> notes.txt
l save -q
AUTO=$(l rev-parse 'HEAD@{0}')
l attest origin "$AUTO" >/dev/null
l ref set pins/live "$AUTO" >/dev/null
l ref set leases/test "$AUTO" --ttl 1s >/dev/null
ok 'live refs survive GC' l gc
ok 'pinned auto state remains readable' l show "$AUTO":notes.txt
sleep 2
no 'expired refs stop resolving' l rev-parse leases/test
ok 'GC removes expired refs' l gc
eq 'expired refs have a signed delete transition' None "$(l ref log leases/test --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["after"])')"
l ref delete pins/live >/dev/null
l gc >/dev/null
no 'an attestation does not prevent subject GC' l show "$AUTO":notes.txt
ok 'reindex preserves core metadata' l reindex -q
eq 'reindex preserves current ref' "$S2" "$(l rev-parse env/prod)"
eq 'reindex preserves change versions' "$REB" "$(l rev-parse changes/$C)"
has 'signed records verify' 'signatures ok' "$(l fsck)"
finish
