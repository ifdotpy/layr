#!/bin/bash
# Copies of fast lines go once no command needs them: read-only shadow snapshots but the newest,
# shared start-state checkouts that no new line used for a day, and the shadow of a deleted
# line. LAYR (built with layrfs-fuse), an isolated LAYR_ROOT and LAYR_SOCKET of its root daemon.
set -euo pipefail
: "${LAYR:?}" "${LAYR_ROOT:?}" "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Sweep LAYR_AUTHOR_EMAIL=sweep@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
work=$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-sweep.XXXXXX")
p=fastsweep-$(date +%s)-$$
mkdir -p $work/src; for i in 1 2 3; do echo $i > $work/src/f$i; done
$L init $p --from $work/src -q >/dev/null
P=$LAYR_ROOT/projects/$p
cd $P/lines/main; $L protect main --remove >/dev/null; $L migrate --delete-old >/dev/null
$L line new a --fast -q; $L line new b --fast -q
cd $P/lines/a
for i in 1 2 3 4; do echo "e$i" >> f1; $L add f1; $L status --porcelain >/dev/null; done
count() { sudo sh -c "ls $1 2>/dev/null | wc -l"; }
id_a=$($L line list --json 2>/dev/null | python3 -c 'import json,sys; print([l["id"] for l in json.load(sys.stdin) if l["name"]=="a"][0])' 2>/dev/null || sudo sh -c "grep -l '\"name\":\"a\"' $P/meta/vlines/*/native.json | xargs dirname | xargs basename")
ro=$P/meta/vlines/$id_a/shadow-ro
before=$(count $ro)
[ "$before" -ge 2 ] || { echo "expected several read-only shadow snapshots, got $before" >&2; exit 1; }
seeds=$(sudo sh -c "ls $P/meta/shadows | grep -vc '\.used$'")
[ "$seeds" -ge 1 ] || { echo "no shared checkout" >&2; exit 1; }
LAYR_FAST_RO_MINUTES=0 $L gc -q
# The view never reads from objects that a gc may remove: the changed file reads after a gc.
[ "$(tail -1 $P/lines/a/f1)" = e4 ] || { echo "the changed file does not read after gc" >&2; exit 1; }
[ "$(cat $P/lines/a/f2)" = 2 ] || { echo "an unchanged file does not read after gc" >&2; exit 1; }
[ "$(count $ro)" = 1 ] || { echo "read-only snapshots left: $(count $ro)" >&2; exit 1; }
$L status --porcelain | grep -q f1
LAYR_FAST_SEED_HOURS=0 $L gc -q
[ "$(sudo sh -c "ls $P/meta/shadows | grep -vc '\.used$'" || true)" = 0 ] || { echo "a shared checkout is left" >&2; exit 1; }
# A new line after the sweep makes the checkout again.
cd $P/lines/main; $L line new c --fast -q; cd $P/lines/c; echo x >> f2; $L status --porcelain | grep -q f2
cd $P/lines/main
for l in a b c; do $L line delete -D --discard $l >/dev/null; done
[ -z "$(sudo sh -c "ls $P/meta/vlines")" ] || { echo "a line folder is left: $(sudo ls $P/meta/vlines)" >&2; exit 1; }
echo "PASS: fast line copies go (read-only snapshots $before -> 1, shared checkouts $seeds -> 0)"
