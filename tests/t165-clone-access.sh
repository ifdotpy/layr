#!/bin/bash
# ci: macos
# The role list is machine-local. A project without one lets everyone write ('*' writer, the old default): a clone
# writes its list without that default; the cloning user is admin. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l remote add archive "file://$T/archive" --paper-key-out "$T/paper.txt" >/dev/null 2>&1
ok "push the project" l push archive -q
B() { env LAYR_ROOT="$T/b/store" LAYR_PAPER_KEY_FILE="$T/paper.txt" "$LAYR" "$@"; }
mkdir -p "$T/b"
B clone "file://$T/archive" p >/dev/null 2>&1
cd "$T/b/store/projects/p/lines/main" || exit 1
eq "a clone gives no role to everyone" 0 "$(B access | grep -c '^\*')"
ok "the cloning user is admin" sh -c "env LAYR_ROOT='$T/b/store' '$LAYR' access | grep -q 'admin'"
B access grant nobody reader >/dev/null
ok "a grant lists its role" sh -c "env LAYR_ROOT='$T/b/store' '$LAYR' access | grep -q '^nobody'"
finish
