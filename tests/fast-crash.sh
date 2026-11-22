#!/bin/bash
# A fast line after a crash of the service (SIGKILL) and after a restart of the machine (the
# service and its mounts are gone): a new service mounts the line again at its start, before
# any command, and the unsaved changes are still there. Linux. Run as a non-root user with sudo;
# LAYR (the binary of the running daemon), an isolated LAYR_ROOT and LAYR_SOCKET of its root
# daemon (running). The test stops that daemon and starts it again (log: CRASH_LOG).
set -u
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}" "${LAYR_SOCKET:?}" "${CRASH_LOG:=${TMPDIR:-/tmp}/layr-crash-daemon.$$.log}"
export LAYR_AUTHOR_NAME=Crash LAYR_AUTHOR_EMAIL=crash@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
fails=0
check() { # check <description> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "ok: $1"; else echo "FAIL: $1"; echo "  expected: $2"; echo "  actual:   $3"; fails=$((fails + 1)); fi
}
start_daemon() {
  # Root writes the log (the folder may be another user's). No descriptor of the caller stays
  # open in the daemon: a caller that reads this test's output would wait for it.
  sudo -b sh -c 'exec env LAYR_ROOT="$1" setsid "$2" daemon --socket "$3" >/dev/null 2>>"$4" </dev/null' sh \
    "$LAYR_ROOT" "$L" "$LAYR_SOCKET" "$CRASH_LOG" >/dev/null 2>&1 </dev/null
  for _ in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && "$L" version >/dev/null 2>&1 && break; sleep 0.1; done
}
kill_daemon() {
  # The pattern must not match this shell's own command line.
  sudo pkill -9 -f "[ ]daemon --socket $LAYR_SOCKET"
  for _ in $(seq 1 50); do pgrep -f "[ ]daemon --socket $LAYR_SOCKET" >/dev/null || break; sleep 0.1; done
  sudo rm -f "$LAYR_SOCKET"
}
# wait_for <path>: up to five seconds for a path in the line (the service mounts it at its start).
wait_for() { for _ in $(seq 1 50); do [ -e "$1" ] && return 0; sleep 0.1; done; return 1; }
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-crash.XXXXXX")" && pwd -P)
p=fastcrash-$(date +%s)-$$
mkdir -p "$work/s"; for i in 1 2 3; do echo $i > "$work/s/f$i"; done
$L init $p --from "$work/s" -q >/dev/null
cd "$LAYR_ROOT/projects/$p/lines/main" && $L protect main --remove >/dev/null && $L migrate --delete-old >/dev/null
$L line new x --fast -q || { echo 'FAIL: line new --fast'; exit 1; }
V=$LAYR_ROOT/projects/$p/lines/x
cd "$V"
echo saved > a; $L add a >/dev/null; $L save -qm a
echo unsaved > b; echo changed >> f1
cd /

for case in crash restart; do
  kill_daemon
  # A restart of the machine: no mount is left.
  [ $case = restart ] && sudo umount -l "$V"
  start_daemon
  wait_for "$V/b"; check "$case: the line is mounted again before any command" "unsaved" "$(cat "$V/b" 2>&1)"
  check "$case: an unsaved change is kept" "$(printf '1\nchanged')" "$(cat "$V/f1" 2>&1)"
  check "$case: status" "$(printf ' M f1\n?? b')" "$(cd "$V" && $L status --porcelain 2>&1)"
  check "$case: the saved head" "a" "$(cd "$V" && $L log -n 1 --format=%s 2>&1)"
done
echo more >> "$V/f1"
check "a save after the restarts" "0" "$(cd "$V" && $L save -aqm after >/dev/null 2>&1; echo $?)"
check "the saved content" "$(printf '1\nchanged\nmore')" "$(cd "$V" && $L show head:f1 2>&1)"
# A command finds a line that the service has not mounted yet (it mounts the line itself).
kill_daemon; sudo umount -l "$V"
start_daemon
cd "$LAYR_ROOT/projects/$p/lines/main"
check "line delete" "0" "$($L line delete -D --discard x >/dev/null 2>&1; echo $?)"
if [ $fails -eq 0 ]; then
  echo "PASS: fast lines come back after a crash and a restart of the service"
else
  echo "FAIL: $fails checks; daemon log: $CRASH_LOG" >&2; exit 1
fi
