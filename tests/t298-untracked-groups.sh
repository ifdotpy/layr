#!/bin/bash
# ci: macos overlayfs
# Untracked files inside an untracked folder are listed as that folder, in status and in
# `ls-files -o --directory`; a sibling whose name only starts like the folder (a/b, a/bc) is not.
. "$(dirname "$0")/lib.sh"

mkdir -p "$T/src/a" "$T/src/k"
printf 'tracked\n' > "$T/src/a/tracked.txt"
printf "tracked\n" > "$T/src/k/tracked.txt"
l init untracked-groups --from "$T/src" -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/untracked-groups/lines/main" || exit 1
mkdir -p a/b a/b/deep new/x/y k/m
printf 1 > a/b/1; printf 1 > a/b/deep/2; printf 1 > a/bc; printf 1 > a/b.txt; printf 1 > a/c
printf 1 > new/x/y/f; printf 1 > new/x/g; printf 1 > k/m/h; printf 1 > k/n; printf 1 > top

expected=$'a/b.txt\na/b/\na/bc\na/c\nk/m/\nk/n\nnew/\ntop'
eq "ls-files -o --directory groups by folder" "$expected" "$(l ls-files -o --directory)"
eq "ls-files -o --directory is the same with -z" "$expected" "$(l ls-files -o --directory -z | tr '\0' '\n')"
eq "ls-files -o lists every file" $'a/b.txt\na/b/1\na/b/deep/2\na/bc\na/c\nk/m/h\nk/n\nnew/x/g\nnew/x/y/f\ntop' "$(l ls-files -o)"
eq "status --porcelain shows the same labels" "$(printf '?? %s\n' $expected)" "$(l status --porcelain | grep '^??')"
finish
