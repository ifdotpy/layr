#!/bin/bash
# ci: macos overlayfs
# Revision failures reach the caller as errors, with diagnostics on stderr.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
echo original > "$T/src/file"
l init revisions --from "$T/src" -q || exit 1
cd "$LAYR_ROOT/projects/revisions/lines/main"
HEAD_BEFORE=$(l rev-parse head) || exit 1
echo working > file
l add file || exit 1
echo unstaged >> file

failed_revision() {
    local code=$1 message=$2 rc
    shift 2
    # Capture the command itself: a pipeline can hide its failing exit status.
    l "$@" > "$T/stdout" 2> "$T/stderr"
    rc=$?
    eq "$* exits $code" "$code" "$rc"
    no "$* leaves stdout empty" test -s "$T/stdout"
    has "$* explains the revision failure on stderr" "$message" "$(cat "$T/stderr")"
}

# The exact benchmark command and both arguments individually must fail.
failed_revision 2 'HEAD was removed. Use head.' diff 'HEAD~10' HEAD
failed_revision 2 'HEAD was removed. Use head.' diff 'HEAD~10' head
failed_revision 2 'HEAD was removed. Use head.' diff head HEAD
failed_revision 2 'HEAD was removed. Use head.' diff 'head..HEAD'
failed_revision 2 'HEAD was removed. Use head.' log 'head...HEAD'

for revision in HEAD 'HEAD~10' 'HEAD@{0}' 'stash@{0}'; do
    case "$revision" in
        HEAD*) hint='HEAD was removed. Use head.' ;;
        *) hint='stash revisions were removed. Use park@{n}.' ;;
    esac
    failed_revision 2 "$hint" diff "$revision" head --
    failed_revision 2 "$hint" show "$revision:file"
    failed_revision 2 "$hint" log "$revision" --
    failed_revision 2 "$hint" restore --source "$revision" -- file
    failed_revision 2 "$hint" export "$revision" "$T/export"
    failed_revision 2 "$hint" objects checkout "$revision" "$T/checkout"
done

# Unknown names and valid syntax with unavailable ancestry fail as well.
for revision in missing-revision 'head~10'; do
    failed_revision 128 "bad revision '$revision'" diff "$revision" head --
    failed_revision 128 "bad revision '$revision'" log "$revision" --
    case "$revision" in
        missing-revision) hint="unknown revision '$revision'" ;;
        *) hint="$revision: state has no parent" ;;
    esac
    failed_revision 128 "$hint" show "$revision:file"
    failed_revision 128 "$hint" restore --source "$revision" -- file
    failed_revision 128 "$hint" export "$revision" "$T/export"
    failed_revision 128 "$hint" objects checkout "$revision" "$T/checkout"
done

eq 'failed revisions keep the head' "$HEAD_BEFORE" "$(l rev-parse head)"
eq 'failed restores keep the working file' $'working\nunstaged' "$(cat file)"
eq 'failed restores keep the staged file' working "$(l show :file)"
no 'failed exports create no destination' test -e "$T/export"
no 'failed checkouts create no destination' test -e "$T/checkout"

# A removed name is rejected before implicit path fallback, but -- permits a path.
echo path > 'HEAD~10'
failed_revision 2 'HEAD was removed. Use head.' diff 'HEAD~10'
failed_revision 2 'HEAD was removed. Use head.' log 'HEAD~10'
ok 'an explicit HEAD~10 diff path is accepted' l diff -- 'HEAD~10'
ok 'an explicit HEAD~10 log path is accepted' l log -- 'HEAD~10'
ok 'valid lowercase head still compares successfully' l diff head head
eq 'valid lowercase head still reads the saved file' original "$(l show head:file)"
finish
