#!/bin/bash
# Commands must operate in the input folder and keep its metadata intact.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
git -C "$T/src" init -q
git -C "$T/src" add -A && git -C "$T/src" commit -qm original || exit 1
GIT_HEAD=$(git -C "$T/src" rev-parse HEAD)
GIT_CONFIG=$(cat "$T/src/.git/config")
l init native --from "$T/src" -q >/dev/null || exit 1
(cd "$LAYR_ROOT/projects/native/lines/main" && l protect main --remove) || exit 1
ok "attach the input folder" l workspace attach native --path "$T/src"
ok "repeat attachment" l workspace attach native --path "$T/src"
cd "$T/src" || exit 1
ok "the locator exists" test -f .layr/link.json
eq "input folder starts clean" "" "$(l status --porcelain)"
printf 'input change\n' > native.txt
ok "stage in the input folder" l add native.txt
ok "commit from the input folder" l commit -qm native
eq "history contains the input change" "input change" "$(l show HEAD:native.txt)"
no "history excludes the locator" l show HEAD:.layr/link.json
ok "create a separate worker line" l branch -q worker
W=$LAYR_ROOT/projects/native/lines/worker
eq "worker receives the committed file" "input change" "$(cat "$W/native.txt")"
printf 'worker change\n' > "$W/native.txt"
eq "worker writes do not change input" "input change" "$(cat native.txt)"
(cd "$W" && l commit -qam worker) || exit 1
ok "merge worker changes into input" l merge worker
eq "input receives the merged file" "worker change" "$(cat native.txt)"
ok "the locator survives merge" test -f .layr/link.json
no "the attached line cannot be renamed" l branch -m main moved
no "another line cannot delete input" env LAYR_PROJECT=native LAYR_LINE=worker "$LAYR" branch -D main
ok "the input folder survives" test -f native.txt
printf 'cache/\n' > .gitignore
printf 'cache regenerable\n' > .layrattributes
l add .gitignore .layrattributes && l commit -qm cache-rules || exit 1
mkdir -p cache
printf 'warm cache\n' > cache/data
ok "save the input folder cache" l save -q --caches
no "cache stays outside saved history" l show 'HEAD@{0}:cache/data'
ok "create a worker with the saved cache" l branch -q warm 'HEAD@{0}'
eq "worker receives the saved cache" "warm cache" "$(cat "$LAYR_ROOT/projects/native/lines/warm/cache/data")"
ok "a build can remove its ordinary cache folder" rm -rf cache
ok "the locator survives cache removal" test -f .layr/link.json
eq "Layr preserves the input Git head" "$GIT_HEAD" "$(git rev-parse HEAD)"
eq "Layr preserves the input Git config" "$GIT_CONFIG" "$(cat .git/config)"
ok "snapshots pass verification" l fsck
# Undoing creation of an attached line must fail before any state is saved.
sample "$T/undo-source"
l init undo-native --from "$T/undo-source" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/undo-native/lines/main" || exit 1
l branch -q attached || exit 1
CREATE=$(l op log --all -n 1 | awk '{print $1}')
l workspace attach undo-native --line attached --path "$T/undo-source" >/dev/null || exit 1
cd "$T/undo-source" || exit 1
RECORDS=$(l op export)
no "undo cannot remove an attached line" l undo "$CREATE"
eq "rejected undo creates no records" "$RECORDS" "$(l op export)"
ok "rejected undo preserves the input" test -f .layr/link.json
ok "the attached line stays usable after rejected undo" l status --porcelain
finish
