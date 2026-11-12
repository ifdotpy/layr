#!/bin/bash
# ci: macos
# `layr remote add` of a server that cannot be reached (no connection, no name) adds the remote with a warning: the
# first fetch or push makes the check (docs/remotes.md). `--mode`, `--paper-key-out` and `--paper-key-tty` need the
# server and still refuse. A definite answer of a reachable server (no project, no access) refuses as before
# (unit test `only_a_server_that_does_not_answer_is_unreachable`). Each unreachable request is tried for about 31 s.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
# A port that nothing listens on.
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
url="layr+https://127.0.0.1:$port/p"

out=$(l remote add origin "$url" 2>&1); code=$?
eq "a remote that cannot be reached is added" 0 "$code"
has "with a warning" "warning: remote origin: cannot reach $url" "$out"
has "that names the later check" "added without a check, the first fetch or push checks it" "$out"
has "remote list shows it" "remote	origin	$url" "$(l remote list 2>&1)"
eq "a second add of the name is refused" 3 "$(l remote add origin "$url" >/dev/null 2>&1; echo $?)"

out=$(l remote add strict "$url" --mode private 2>&1); code=$?
no "--mode needs the server: the add is refused" test "$code" = 0
has "and names the reason" "error sending request" "$out"
no "and adds no remote" grep -q strict <<<"$(l remote list 2>&1)"
finish
