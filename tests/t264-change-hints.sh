#!/bin/bash
# ci: macos overlayfs
# Native status captures keep local ignore rules, including nested .git subvolumes.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
printf 'tracked\n' > "$T/src/file"
l init hints --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/hints/lines/main" || exit 1
if [ "$(uname -s)" = Linux ] && [ "${LAYR_TEST_BACKEND:-}" != overlayfs ]; then
    btrfs subvolume create .git >/dev/null || exit 1
fi
mkdir -p .git/info
printf 'local\n' > .git/info/exclude
printf 'ignored\n' > local
eq "native capture keeps external local ignores" '!! local' "$(l status --porcelain --ignored)"
printf 'changed\n' > file
eq "native capture reports only changed and ignored paths" ' M file
!! local' "$(l status --porcelain --ignored)"
rm .git/info/exclude
eq "removing local policy makes the path untracked" ' M file
?? local' "$(l status --porcelain --ignored)"
finish
