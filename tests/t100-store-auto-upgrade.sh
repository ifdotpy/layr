#!/bin/bash
# ci: overlayfs macos-plain
# A store without a version marker (format 0) with the layout of format 1 gets its marker on the
# first open, with no extra command. A folder that is not a layr store, a store that is not
# writable, and parallel first opens are checked too.
. "$(dirname "$0")/lib.sh"
# Stores under the test root: on Linux, init creates btrfs subvolumes, so the root is the btrfs mount.
ROOTFS=$LAYR_ROOT
unset LAYR_ROOT
export LAYR_DIRECT=1

r() { "$LAYR" --root "$@"; }

state() { # state <store>: the state, compatible and upgrade fields of store-version --json
    "$LAYR" --root "$1" store-version --json | python3 -c 'import json,sys; r=json.load(sys.stdin); print(r["state"], r["compatible"], r["upgrade"] is not None)'
}

same_identity() { # same_identity <store>: the marker equals the binary identity
    "$LAYR" --root "$1" store-version --json | python3 -c 'import json,sys; r=json.load(sys.stdin); m=json.load(open(sys.argv[1])); sys.exit(0 if r["store"] == r["binary"] == m else 1)' "$1/store-version.json"
}

S=$ROOTFS/auto
ok "init creates a project in a new store" r "$S" init demo -q
ok "a new store writes its marker" test -f "$S/store-version.json"

rm "$S/store-version.json"
eq "a store without a marker is reported as unmarked" "unmarked True True" "$(state "$S")"
ok "the first command opens the unmarked store" r "$S" projects
ok "the open writes the marker" test -f "$S/store-version.json"
ok "the marker records this binary" same_identity "$S"
eq "the store is compatible after the open" "compatible True False" "$(state "$S")"
written=$(stat_field inode "$S/store-version.json")
ok "a second command works" r "$S" init second -q
eq "the second command does not rewrite the marker" "$written" "$(stat_field inode "$S/store-version.json")"

F=$T/foreign
mkdir "$F" && echo bytes > "$F/evidence"
before=$(find "$F" | sort)
out=$(r "$F" projects 2>&1); rc=$?
eq "a folder without a layr machine identity is refused" 128 "$rc"
has "the refusal names the missing machine identity" "no machine/id" "$out"
eq "the refused folder is not changed" "$before" "$(find "$F" | sort)"

P=$ROOTFS/parallel
ok "init creates a project in the parallel store" r "$P" init demo -q
rm "$P/store-version.json"
pids=()
for i in 1 2 3 4 5 6 7 8; do
    ( r "$P" projects > "$T/parallel-$i.out" 2>&1 ) &
    pids+=($!)
done
codes=()
for pid in "${pids[@]}"; do
    wait "$pid"
    codes+=($?)
done
eq "all parallel first opens succeed" "0 0 0 0 0 0 0 0" "${codes[*]}"
ok "the marker is valid after the parallel opens" same_identity "$P"
ok "no temporary marker is left" test -z "$(find "$P" -name '.store-version.json.tmp-*')"
ok "no parallel open reports an incompatible store" test -z "$(grep -l incompatible "$T"/parallel-*.out)"

printf '{"format":0,"version":"0.0.0","build":"hand"}' > "$P/store-version.json"
no "an explicit format 0 marker is refused, not upgraded" r "$P" projects

if [ "$(id -u)" = 0 ]; then
    echo "SKIP a store that is not writable (root ignores permission bits)"
else
    W=$ROOTFS/readonly
    ok "init creates a project in the read-only store" r "$W" init demo -q
    rm "$W/store-version.json"
    before=$(find "$W" | sort)
    chmod u-w "$W"
    out=$(r "$W" projects 2>&1); rc=$?
    chmod u+w "$W"
    eq "a store that is not writable is refused" 128 "$rc"
    has "the refusal names write access" "give write access" "$out"
    eq "the refused store is not changed" "$before" "$(find "$W" | sort)"
fi

finish
