#!/bin/bash
# ci: macos overlayfs
# A configured cache below a symlink must neither follow it nor fail after line.delete.
. "$(dirname "$0")/lib.sh"

sample "$T/src"
printf 'sub/\n' >> "$T/src/.gitignore"
l init cachepaths --from "$T/src" -q >/dev/null || exit 1
P=$LAYR_ROOT/projects/cachepaths
cd "$P/lines/main" || exit 1
l overlay create nested --folder sub/cache -q || exit 1
l line new -q victim || exit 1
mkdir -p "$T/external/cache"
printf 'outside\n' > "$T/external/cache/keep"
# Replace the ancestor, leaving the external cache completely outside this line.
[ ! -e "$P/lines/victim/sub" ] || mv "$P/lines/victim/sub" "$P/lines/main/detached"
ln -s "$T/external" "$P/lines/victim/sub"
out=$(LAYR_TRACE=1 l line delete -D --discard victim 2>&1); rc=$?
[ "$rc" -eq 0 ] || printf '%s\n' "$out"
eq "delete succeeds with a cache below a symlink" 0 "$rc"
no "the deleted working folder is gone" test -e "$P/lines/victim"
eq "detachment does not follow the symlink" outside "$(cat "$T/external/cache/keep")"
eq "one line deletion was committed" 1 "$(l op log --all --json | python3 -c 'import json,sys; print(sum(json.loads(row)["op"] == "line.delete" for row in sys.stdin))')"
no "post-commit cleanup reports no symbolic-link error" grep -q 'path goes through a symbolic link' <<<"$out"

finish
