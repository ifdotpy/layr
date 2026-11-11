#!/bin/bash
# ci: macos overlayfs
# Cache overlays and their removal (docs/cache-overlays.md, sections 3 and 7). A cache overlay
# applies only in lines where its folder is a cache path: `overlay list` and `overlay repair` agree
# there and saves ignore it. `overlay remove` drops a cache overlay from the project; a versioned
# overlay is refused. One user, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf '.layr-retest/\n' >> $T/src/.gitignore
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/overlays
cd $P/main
l protect main --remove >/dev/null 2>&1 || true
# The name and the state of the folder overlay of line a (the overlay name is a hash here).
name_of() { l overlay list | grep 'folder=\.layr-retest' | cut -f1 | sed 's/^[* ]*//'; }
state_of() { l overlay list | grep 'folder=\.layr-retest' | cut -f6; }

# 1. Line a marks the folder regenerable and saves its cache: the overlay follows line a.
l line new a main >/dev/null
cd $P/a
echo '.layr-retest regenerable' > .layrattributes
mkdir -p .layr-retest && echo cached > .layr-retest/out.bin
out=$(l save --glasses)
has "save --glasses names the folder overlay" ".layr-retest: overlay" "$out"
name=$(name_of)
eq "line a has the cache" "cache" "$(state_of)"

# 2. Line main does not mark the folder: list and repair agree, with no repair hint.
cd $P/main
out=$(l overlay list)
has "main: overlay list says the folder is no cache path" "not a cache path in this line" "$out"
eq "main: overlay list gives no repair hint" "0" "$(echo "$out" | grep -c 'overlay repair')"
out=$(l overlay repair "$name")
rc=$?
eq "main: overlay repair exits 0" "0" "$rc"
has "main: overlay repair says the folder is no cache path" "not a cache path in this line" "$out"
eq "main: status is clean" "" "$(l status --short)"
echo more >> notes.txt
ok "main: a named save works" l save -a -m "main change" -q
ok "main: save --glasses works" l save --glasses -q
eq "main: the cache overlay of line a is still there for line a" "cache" "$(cd $P/a && state_of)"

# 3. A versioned overlay is refused by remove, with a reason.
l overlay create keys -q >/dev/null
no "overlay remove refuses a versioned overlay" l overlay remove keys
out=$(l overlay remove keys 2>&1)
has "the refusal says the overlay is versioned" "versioned overlay" "$out"

# 4. Remove the cache overlay: the folder stays in line a as a plain folder with its files.
states_before=$(ls "$O/$name/states" | wc -l | tr -d ' ')
ok "overlay remove removes the cache overlay" l overlay remove "$name"
eq "the folder of line a keeps its files" "cached" "$(cat $P/a/.layr-retest/out.bin)"
eq "no line shows the overlay (line a)" "0" "$(cd $P/a && l overlay list | grep -c 'folder=\.layr-retest')"
eq "no line shows the overlay (main)" "0" "$(l overlay list | grep -c 'folder=\.layr-retest')"
eq "remove names no overlay of the folder any more" "" "$(cd $P/a && name_of)"

# 5. A new line does not get the overlay.
l line new fresh main >/dev/null
eq "a new line has no cache folder" "" "$(ls $P/fresh/.layr-retest 2>/dev/null)"

# 6. The states of the removed overlay are freed by gc --repair; only its empty first state stays.
ok "the overlay had states to free" test "$states_before" -gt 1
l gc --repair -q >/dev/null
eq "gc frees the states of the removed overlay" "1" "$(ls "$O/$name/states" | wc -l | tr -d ' ')"

# 7. A new save --glasses of the folder makes a cache overlay again.
cd $P/a
out=$(l save --glasses)
has "save --glasses makes the overlay again" ".layr-retest: overlay" "$out"
eq "line a shows the new cache" "cache" "$(state_of)"

finish
