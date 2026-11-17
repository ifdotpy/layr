#!/bin/bash
# ci: macos overlayfs
# Projects made with --objects write source states to objects on every backend; legacy projects keep
# their setting, and `clone` always enables objects.
. "$(dirname "$0")/lib.sh"

json_field() {
    python3 -I -c 'import json,sys; print(json.loads(sys.argv[2])[sys.argv[1]])' "$1" "$2"
}

check_save() {
    local label=$1
    l protect main --remove >/dev/null
    echo first > first.txt
    l add first.txt
    ok "$label: first save" l save -m first -q
    local st
    st=$(l objects stat --json)
    eq "$label: object format" 2 "$(json_field format "$st")"
    ok "$label: first save writes objects" test "$(json_field states "$st")" -gt 0
    ok "$label: saved head has an object checkout" l objects checkout head "$T/checkout-$label"
    eq "$label: object checkout has the saved content" first "$(cat "$T/checkout-$label/first.txt")"
    local status
    status=$(l status --short)
    eq "$label: status succeeds" 0 "$?"
    eq "$label: clean status" "" "$status"
    echo second >> first.txt
    has "$label: status sees an edit" first.txt "$(l status --short)"
    has "$label: diff reads the object state" +second "$(l diff)"
    l line new -q topic head >/dev/null
    eq "$label: line new reads the saved state" first "$(cat ../topic/first.txt)"
    eq "$label: new line is clean" "" "$(cd ../topic && l status --short)"
}

l init empty --objects -q >/dev/null
cd "$LAYR_ROOT/projects/empty/lines/main"
eq "empty init records its initial state as objects" 1 "$(json_field states "$(l objects stat --json)")"
check_save 'init'

mkdir -p "$T/src"
echo source > "$T/src/source.txt"
l init imported --objects --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/imported/lines/main"
eq "init --from records its initial state as objects" 1 "$(json_field states "$(l objects stat --json)")"
eq "init --from preserves the source" source "$(l show head:source.txt)"
check_save 'init --from'

# Git imports keep their lazy Git history; the first new save must use objects.
git init -q -b main "$T/git"
echo git > "$T/git/source.txt"
git -C "$T/git" add source.txt
git -C "$T/git" commit -qm initial
l git clone "$T/git" git-copy --objects -q >/dev/null
cd "$LAYR_ROOT/projects/git-copy/lines/main"
eq "git clone enables objects before the first save" true "$(l config objects.enabled)"
check_save 'git clone'

# Migration tests still need real v1 fixtures. Opening an existing v1 project must not enable v2.
l init legacy --no-objects --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/legacy/lines/main"
l config objects.enabled >/dev/null
no "opening a legacy project makes no object store" test -e "$LAYR_ROOT/projects/legacy/meta/objects"
# Clone the v1 records, including an explicit false setting, to a fresh local replica.
l config objects.enabled false >/dev/null
l remote add cloud "file://$T/archive" --paper-key-out "$T/paper" >/dev/null
clone_root=$T/clone-store
read -r machine key <<<"$(LAYR_ROOT="$clone_root" "$LAYR" remote self --json | python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(d["id"], d["public_key"])')"
l remote trust cloud "$machine" --public-key "$key" >/dev/null
l push cloud -q >/dev/null
export LAYR_ROOT=$clone_root
l clone "file://$T/archive" replica -q >/dev/null
cd "$LAYR_ROOT/projects/replica/lines/main"
eq "clone overrides the source's v1 setting" true "$(l config objects.enabled)"
check_save 'clone'
finish
