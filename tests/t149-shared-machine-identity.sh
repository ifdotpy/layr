#!/bin/bash
# ci: macos
# Two stores with one machine identity (docs/remote-format.md): each continues the machine's records
# from the remote, and the push of the second writer fails loudly instead of dropping its records.
# The remote stays readable.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
l push archive -q >/dev/null 2>&1
segments() { find "$T/archive/records" -type f | wc -l | tr -d ' '; }
for m in b c; do
  mkdir -p "$T/$m"; cp -R "$LAYR_ROOT/machine" "$T/$m/machine"
  env LAYR_ROOT="$T/$m" "$LAYR" clone "file://$T/archive" p >/dev/null 2>&1
done
work() { # work <store> <text>: one saved change in line main of that store
  (cd "$T/$1/projects/p/lines/main" && echo "$2" > "f-$2.txt" && env LAYR_ROOT="$T/$1" "$LAYR" add "f-$2.txt" >/dev/null && env LAYR_ROOT="$T/$1" "$LAYR" save -qm "$2" >/dev/null)
}
push() { (cd "$T/$1/projects/p/lines/main" && env LAYR_ROOT="$T/$1" "$LAYR" push origin 2>&1); }

work b one
ok "the first store pushes the machine's next records" push b
ok "a second push of that store is fine" push b
work b two
ok "and so is a push of new work" push b
n=$(segments)
work c three
out=$(push c); code=$?
ok "the push of the other store with the same identity fails" test "$code" != 0
has "it names the cause" "another store uses this machine identity" "$out"
eq "it publishes no records" "$n" "$(segments)"
work c four
no "a later push of that store fails too" push c
work b five
ok "the first store goes on" push b
mkdir -p "$T/d"; cp -R "$LAYR_ROOT/machine" "$T/d/machine"
ok "a fresh store with the identity clones the remote" env LAYR_ROOT="$T/d" "$LAYR" clone "file://$T/archive" p
ok "and has the work of the first store" test -f "$T/d/projects/p/lines/main/f-five.txt"
work d six
ok "and continues its records" push d
finish
