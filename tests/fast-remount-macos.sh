#!/bin/bash
# macOS: a fast line whose mount is gone (a restart of the machine) is mounted again by the next
# command that reads it, with its unsaved changes; a deleted line is not mounted again. Needs the
# LayrFS extension; LAYR and an isolated LAYR_ROOT. Run as the user who owns the lines.
set -u
[ "$(uname -s)" = Darwin ] || { echo 'PASS: macOS only'; exit 0; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
export LAYR_AUTHOR_NAME=Remount LAYR_AUTHOR_EMAIL=remount@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
fails=0
check() { # check <description> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "ok: $1"; else echo "FAIL: $1"; echo "  expected: $2"; echo "  actual:   $3"; fails=$((fails + 1)); fi
}
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-remount.XXXXXX")" && pwd -P)
p=fastremount-$(date +%s)-$$
mkdir -p "$work/s"; printf 'one\n' > "$work/s/f1"
$L init $p --from "$work/s" -q >/dev/null
cd "$LAYR_ROOT/projects/$p/lines/main" && $L protect main --remove >/dev/null
$L line new x --fast -q || { echo 'FAIL: line new --fast'; exit 1; }
V=$LAYR_ROOT/projects/$p/lines/x
printf 'unsaved\n' > "$V/u1"
cd /
umount "$V" || { echo 'FAIL: umount'; exit 1; }
check "the folder is empty without its mount" "0" "$(ls -A "$V" | wc -l | tr -d ' ')"
check "status reads the line" "?? u1" "$(cd "$V" && $L status --porcelain 2>&1)"
check "the line is mounted again" "unsaved" "$(cat "$V/u1" 2>&1)"
check "a file of the start state" "one" "$(cat "$V/f1" 2>&1)"
cd "$LAYR_ROOT/projects/$p/lines/main"
check "line delete" "0" "$($L line delete -D --discard x >/dev/null 2>&1; echo $?)"
check "the deleted line has no folder" "no" "$([ -e "$V" ] && echo yes || echo no)"
if [ $fails -eq 0 ]; then
  echo "PASS: a fast line comes back after its mount is gone"
else
  echo "FAIL: $fails checks" >&2; exit 1
fi
