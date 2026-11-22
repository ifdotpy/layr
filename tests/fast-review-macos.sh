#!/bin/bash
# macOS fast lines (review fixes): a rename of a start-state file over another keeps the right
# content; a rename that changes only the case of a name gives the status of a classic line; a
# folder that got files while its mount was gone stops status; an open removed file takes ftruncate;
# line delete unmounts first.
# Needs the LayrFS extension; LAYR and an isolated LAYR_ROOT. Run as the user who owns the lines.
set -u
[ "$(uname -s)" = Darwin ] || { echo 'PASS: macOS only'; exit 0; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
export LAYR_AUTHOR_NAME=Review LAYR_AUTHOR_EMAIL=review@example.invalid GIT_CONFIG_GLOBAL=/dev/null
L=$LAYR
fails=0
check() { # check <description> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "ok: $1"; else echo "FAIL: $1"; echo "  expected: $2"; echo "  actual:   $3"; fails=$((fails + 1)); fi
}
work=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/layr-fast-review.XXXXXX")" && pwd -P)
p=fastreview-$(date +%s)-$$
mkdir -p "$work/s"; printf 'one\n' > "$work/s/a"; printf 'two\n' > "$work/s/b"; printf 'doc\n' > "$work/s/README"
$L init $p --from "$work/s" -q >/dev/null
M=$LAYR_ROOT/projects/$p/lines/main
cd "$M" && $L protect main --remove >/dev/null
$L line new x --fast -q || { echo 'FAIL: line new --fast'; exit 1; }
V=$LAYR_ROOT/projects/$p/lines/x
cd "$V"
mv a b
check "a rename over a start-state file" "one" "$(cat b)"
mv README readme
check "a case rename shows one name" "readme" "$(ls | grep -i readme)"
# As in a classic line: the old spelling is a removed file, the new one a new file.
check "status of the renames" " D README
 D a
 M b
?? readme" "$($L status --porcelain 2>&1)"
check "save" "0" "$($L save -aqm renames >/dev/null 2>&1; echo $?)"
check "files of the saved state" "b" "$($L ls-files 2>&1 | tr '\n' ' ' | sed 's/ $//')"
printf 'later\n' > "$V/u1"
cd /
umount "$V" || { echo 'FAIL: umount'; exit 1; }
printf 'stray\n' > "$V/stray"
out=$(cd "$M" && $L --line x status --porcelain 2>&1; echo "rc=$?")
check "status stops on a nonempty unmounted folder" "yes" "$(grep -q 'is not empty' <<<"$out" && echo yes || echo no)"
rm "$V/stray"
check "status mounts it again" "?? readme
?? u1" "$(cd "$V" && $L status --porcelain 2>&1)"
# A removed file that is still open takes a new size through its descriptor.
printf 'twelve bytes' > "$V/open-rm"
check "ftruncate of an open removed file" "0" "$(cd "$V" && perl -e 'open(my $f, "+<", "open-rm") or die "open: $!"; unlink "open-rm" or die "unlink: $!"; truncate($f, 0) or die "truncate: $!"; print((stat($f))[7])' 2>&1)"
cd "$M"
out=$($L line delete -D --discard x 2>&1); rc=$?
check "line delete" "0" "$rc"; [ $rc = 0 ] || echo "$out"
check "the deleted line has no folder" "no" "$([ -e "$V" ] && echo yes || echo no)"
if [ $fails -eq 0 ]; then
  echo "PASS: fast line review fixes"
else
  echo "FAIL: $fails checks" >&2; exit 1
fi
