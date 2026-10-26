#!/bin/bash
# Refs, attestations, logical changes, and GC roots.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
S1=$(l rev-parse head)
no 'the layer alias is removed' l layer list
has 'layer names cache' 'layr cache' "$(l layer list 2>&1)"
removed_cli() {
    local replacement=$1 output rc
    shift
    output=$(l "$@" 2>&1); rc=$?
    eq "$* exits 2" 2 "$rc"
    has "$* names $replacement" "$replacement" "$output"
}
removed_cli 'layr line' branch
removed_cli 'layr save -m' commit
removed_cli 'layr line new' checkout main
removed_cli 'layr line new' switch main
removed_cli 'layr park' stash
removed_cli 'layr pick' cherry-pick
removed_cli 'layr replay' rebase
removed_cli 'layr invert' revert
removed_cli '--no-save' merge --no-commit head
removed_cli '--no-save' pick --no-commit head
removed_cli '--no-save' invert --no-commit head
removed_cli '--autopark' replay --autostash head
removed_cli '--caches' save --layers
removed_cli '--all-lines' save --all
for revision in HEAD 'HEAD~1' 'HEAD^2' 'HEAD@{0}' 'HEAD:notes.txt' 'HEAD^{tree}'; do
    removed_cli head rev-parse "$revision"
done
removed_cli 'park@{n}' rev-parse 'stash@{0}'
for reserved in HEAD head; do
    no "a line cannot use $reserved" l line new "$reserved"
    no "a tag cannot use $reserved" l tag "$reserved"
    has "a ref cannot use $reserved" 'reserved revision name' "$(l ref set "$reserved" head 2>&1)"
done
BATCH=$(l line new batch-default --count 2 --json)
eq 'batch creation uses the current head without a revision' "$S1" "$(printf '%s' "$BATCH" | python3 -c 'import json,sys,uuid; x=json.load(sys.stdin); assert len(x)==2; h=uuid.UUID(x[0]["head"]).bytes; print((h[10:]+h[6:10]+h[:6]).hex()); assert x[0]["head"]==x[1]["head"]')"
no 'line list cannot delete a line' l line list -D batch-default-1
ok 'the list rejection keeps the line folder' test -d ../batch-default-1
(cd ../batch-default-1 && echo grouped > grouped.txt && l add grouped.txt && l save -qm grouped)
ok 'grouped -D deletes an unmerged line' l line delete -qD batch-default-1 batch-default-2
echo first >> notes.txt
l save -qam first
S2=$(l rev-parse head)
C=$(l change list --json | python3 -c 'import json,sys; print(next(x["change_id"] for x in json.load(sys.stdin) if x["message"]=="first"))')
no 'unprefixed ref names fail' l ref set foo head
has 'the error lists all mutable ref prefixes' 'env/, release/, pins/, holds/, leases/, caches/, derived/' "$(l ref set foo head 2>&1)"
ok 'a prefixed ref name works' l ref set env/x head
ok 'set a ref' l ref set env/prod head
no 'ref set refuses a line' l ref set main head~1
no 'a stale expected position fails' l ref set env/prod head~1 --expect head~1
eq 'failed CAS leaves the position' "$S2" "$(l rev-parse env/prod)"
eq 'failed CAS writes no history' 1 "$(l ref log env/prod --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok 'matching CAS moves the ref' l ref set env/prod head~1 --expect head
eq 'ref current position' "$S1" "$(l rev-parse 'env/prod@{0}')"
eq 'ref previous position' "$S2" "$(l rev-parse 'env/prod@{1}')"
ok 'undo the ref move' l undo
eq 'undo restores the ref' "$S2" "$(l rev-parse env/prod)"
no 'delete CAS fails without change' l ref delete env/prod --expect head~1
ok 'delete CAS succeeds' l ref delete env/prod --expect head
no 'deleted ref does not resolve' l rev-parse env/prod
ok 'undo ref delete' l undo
eq 'undo delete restores the ref' "$S2" "$(l rev-parse env/prod)"
eq 'prefix list' env/prod "$(l ref list env/ --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["name"])')"
for prefix in env release pins holds leases caches derived changes views attestations; do
    no "reserved line prefix $prefix" l line new "$prefix/name"
done
has 'views reject malformed identifiers' 'invalid view id' "$(l rev-parse views/unknown 2>&1)"
no 'synthetic revisions cannot be mutable refs' l ref set changes/other head
l ref set pins/cas "$S2" >/dev/null
(l ref set pins/cas "$S1" --expect "$S2" >/dev/null 2>&1; echo $? > "$T/cas1") &
(l ref set pins/cas "$S1" --expect "$S2" >/dev/null 2>&1; echo $? > "$T/cas2") &
wait
eq 'concurrent CAS permits one writer' '0 128' "$(sort -n "$T/cas1" "$T/cas2" | xargs)"

printf 'private payload bytes' > "$T/payload"
A=$(l attest origin head --payload "$T/payload" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["attestation_id"])')
eq 'attestation revision resolves its subject' "$S2" "$(l rev-parse attestations/$A)"
eq 'attestation payload digest' "$(sha256sum "$T/payload" | cut -d' ' -f1)" "$(l attestations head --kind origin --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["payload_digest"])')"
no 'invalid kind fails' l attest unknown head
ok 'approve records an attestation' l approve -q head
eq 'approval kind exists' approval "$(l attestations head --kind approval --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["kind"])')"
ok 'withdraw adds a revoke attestation' l approve -q --withdraw head
eq 'the original attestation remains' 1 "$(l attestations head --kind approval --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
eq 'revoke exists' 1 "$(l attestations head --kind revoke --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
ok 'try writes a passing check attestation' l try -q head -- sh -c 'test -f notes.txt'
no 'try returns the failed command exit code' l try -q head -- sh -c 'exit 7'
eq 'both checks are recorded' 2 "$(l attestations head --kind check --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')"
eq 'change JSON exposes passing and failed command outcomes' '0,7' "$(l change show "$C" --json | python3 -c 'import json,sys; print(",".join(str(c["exit"]) for c in json.load(sys.stdin)["versions"][0]["checks"]))')"
for old in approvals checks; do
    has "$old names its replacement" 'layr attestations' "$(l "$old" head 2>&1)"
    no "$old fails" l "$old" head
done
echo second >> notes.txt
l save -qam amended --amend
S3=$(l rev-parse head)
eq 'amend keeps ChangeId' "$S3" "$(l rev-parse changes/$C)"
eq 'first change version stays addressable' "$S2" "$(l rev-parse changes/$C/v1)"
eq 'second change version resolves' "$S3" "$(l rev-parse changes/$C/v2)"
has 'interdiff shows the amended patch' second "$(l change diff "$C" --from v1 --to v2)"
eq 'change show includes attestations for the earlier version' 1 "$(l change show "$C" --json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["versions"][0]["attestations"])>0 and 1)')"
has 'change log records amend' state "$(l change log "$C")"
no 'version zero fails' l rev-parse changes/$C/v0
l line new -q copied "$S1"
cd ../copied
l pick "$S3" >/dev/null
PICK=$(l rev-parse head)
eq 'cherry-pick keeps ChangeId' "$PICK" "$(l rev-parse changes/$C)"
l line new -q base "$S1"
cd ../base
echo base > base.txt
l add base.txt
l save -qm base
cd ../copied
l replay base >/dev/null
REB=$(l rev-parse head)
eq 'rebase keeps ChangeId' "$REB" "$(l rev-parse changes/$C)"
eq 'base-only rebase changes do not enter the interdiff' '' "$(l change diff "$C" --from v3 --to v4)"
l line new -q merge-source
cd ../merge-source
echo side > side.txt
l add side.txt
l save -qm side
cd ../copied
echo own > own.txt
l add own.txt
l save -qm own
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
AUTO=$(l rev-parse 'head@{0}')
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

NAMED_HEAD=$(l rev-parse head)
l line new -q auto-peer
echo local-auto >> notes.txt
(cd ../auto-peer && echo peer-auto >> notes.txt)
ok 'save --all-lines saves every line' l save -q --all-lines --note 'all line note'
eq 'auto save keeps the named head' "$NAMED_HEAD" "$(l rev-parse head)"
eq 'the peer keeps its named head' "$NAMED_HEAD" "$(cd ../auto-peer && l rev-parse head)"
eq 'the current auto state has its note' 'all line note' "$(l show -s --format=%s 'head@{0}')"
eq 'the peer auto state has its note' 'all line note' "$(cd ../auto-peer && l show -s --format=%s 'head@{0}')"
eq 'the current auto state has its bytes' local-auto "$(l show 'head@{0}:notes.txt' | tail -1)"
eq 'the peer auto state has its bytes' peer-auto "$(cd ../auto-peer && l show 'head@{0}:notes.txt' | tail -1)"
echo single-auto >> notes.txt
ok 'save --note creates an auto state' l save -q --note 'single line note'
eq 'a note does not move the named head' "$NAMED_HEAD" "$(l rev-parse head)"
eq 'the new auto state has its note' 'single line note' "$(l show -s --format=%s 'head@{0}')"
eq 'the other line keeps its auto state' 'all line note' "$(cd ../auto-peer && l show -s --format=%s 'head@{0}')"
finish
