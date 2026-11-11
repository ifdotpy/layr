#!/bin/bash
# ci: macos
# macOS stamps com.apple.provenance on files that a quarantined program writes. It is not user data:
# a file written again with equal content and that attribute is not a change. Another attribute
# still is. (A quarantined layr binary stamped it on every file of a new line: 541 files ' M'.)
. "$(dirname "$0")/lib.sh"
[ "$(uname)" = Darwin ] || { echo "$(basename "$0") (macOS only): 0 passed, 0 failed"; exit 0; }
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l line new b -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/b" || exit 1
f=$(find . -type f ! -path "./.layr*" | head -1); f=${f#./}
# Write the same bytes again as a new file with the same time, as a copy does.
rewrite() { cat "$1" > "$1.new" && touch -r "$1" "$1.new" && mv "$1.new" "$1"; }
rewrite "$f"
xattr -wx com.apple.provenance 0102030405060708 "$f"
ok "a file written again with equal content and the provenance attribute is not changed" test -z "$(l status 2>&1 | grep -F "$f")"
rewrite "$f"
xattr -w user.note kept "$f"
ok "another extended attribute is still a change" test -n "$(l status 2>&1 | grep -F "$f")"
finish
