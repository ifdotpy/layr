#!/bin/bash
# ci: macos apfs-case overlayfs
# `diff -- <path>` compares the selected paths, not the whole working tree, with the same output.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src/dir"
for n in $(seq 1 40); do echo "file $n" > "$T/src/f$n"; done
echo target > "$T/src/target"
echo inside > "$T/src/dir/inside"
l init selected --from "$T/src" -q || exit 1
cd "$LAYR_ROOT/projects/selected/lines/main"

# Every other tracked file changes too: a whole-tree comparison reads each of them.
for n in $(seq 2 40); do echo "changed $n" > "f$n"; done
echo tarGet > target # one size: confirming it reads both sides
same_as_whole() { # same_as_whole <description> <diff arguments...> -- <path>: the output of the full diff, filtered
    local d=$1 path=${*: -1}; shift
    eq "$d" "$(l diff --name-status "${@:1:$#-2}" | awk -F'\t' -v p="$path" '$NF == p || index($NF, p "/") == 1')" \
        "$(l diff --name-status "$@")"
}
same_as_whole 'diff -- <file> lists what the whole diff lists for it' -- target
same_as_whole 'diff -- <unchanged folder> lists nothing' -- dir
has 'diff -- <file> shows its change' '+tarGet' "$(l diff -- target)"
ok 'diff --exit-code -- <unchanged folder> succeeds' l diff --exit-code -- dir
no 'diff --exit-code -- <changed file> fails' l diff --exit-code -- target

if command -v strace >/dev/null; then
    names_other_files() {
        env LAYR_DIRECT=1 LAYR_SOCKET=/nonexistent/layr.sock strace -f -qq -o "$T/trace" "$LAYR" diff "$@" >/dev/null 2>&1 ||
            { echo "diff $* failed"; return; }
        grep -cE '[/"]f[0-9]+"' "$T/trace"
    }
    # On an OverlayFS line the read-only snapshot taken before any comparison of the working folder (as for
    # `status`) copies its upper layer, every changed file: the counts are of the comparison, so not there.
    if [ "$(findmnt -rn -o FSTYPE -T . 2>/dev/null)" != overlay ]; then
        eq 'diff -- <file> reads no other file' 0 "$(names_other_files -- target)"
        eq 'diff -- <folder> reads no file outside it' 0 "$(names_other_files -- dir)"
    fi
    # A large selected file would be read whole: the whole comparison (with filesystem hints, if any) runs instead.
    head -c 2097152 /dev/zero > big; l add big >/dev/null && l save -qm 'big' || exit 1
    ok 'diff -- <large unchanged file> uses the whole comparison' test "$(names_other_files -- big)" -gt 0
    eq 'diff -- <large unchanged file> is empty' '' "$(l diff -- big)"
    ok 'diff --exit-code -- <large unchanged file> finds no change' l diff --exit-code -- big
fi

# A file that cannot be read (here: no read permission; root reads it) only fails a diff that needs its bytes.
if [ "$(id -u)" != 0 ]; then
    chmod 000 f3
    ok 'diff -- <file> succeeds beside an unreadable changed file' l diff -- target
    no 'diff -- <unreadable changed file> fails' l diff -- f3
    chmod 644 f3
fi

# Moves across the selection: a literal path lists what the same path as a pattern (whole trees) lists.
like_pattern() { # like_pattern <diff arguments...> -- <literal> <pattern>
    local pattern=${*: -1} literal=${*: -2:1}
    eq "diff ${*:1:$#-1} lists what $pattern lists" "$(l diff --name-status "${@:1:$#-2}" "$pattern")" \
        "$(l diff --name-status "${@:1:$#-1}")"
}
mv f1 dir/moved
like_pattern -- dir 'dir/*'
like_pattern -- f1 'f[1]'
l add f1 dir/moved >/dev/null || exit 1
like_pattern --cached -- dir 'dir/*'
like_pattern --cached -- f1 'f[1]'
eq 'the whole staged diff pairs the move' "R100	f1	dir/moved" "$(l diff --cached --name-status | grep dir/)"
# A name in another Unicode normalisation is another path, even where the volume finds the file by it (APFS).
nfc=$(printf 'caf\303\251') nfd=$(printf 'cafe\314\201')
echo before > "$nfc"; l add "$nfc" >/dev/null && l save -qm 'nfc' || exit 1
echo after > "$nfc"
eq 'diff -- <NFC name> shows its change' "M	$nfc" "$(l diff --name-status -- "$nfc")"
eq 'diff -- <NFD spelling of it> is empty, as the whole diff filtered' '' "$(l diff --name-status -- "$nfd")"
ok 'diff --exit-code -- <NFD spelling of it> finds no change' l diff --exit-code -- "$nfd"
# U+037E (Greek question mark) is canonically `;`: a volume that ignores normalisation finds one by the other.
greek=$(printf '\315\276')
echo before > "a$greek"; l add "a$greek" >/dev/null && l save -qm 'greek' || exit 1
echo after > "a$greek"
ok 'diff --exit-code -- <ASCII alias of a U+037E name> finds no change' l diff --exit-code -- 'a;'
eq 'diff -- <the U+037E name> finds its change' "M	a$greek" "$(l diff --name-status -- "a$greek")"
long=$(printf 'x%.0s' $(seq 1 300))
eq 'diff -- <name too long> is empty, as the whole diff' '' "$(l diff -- "$long")"
ok 'diff -- <name too long> succeeds, as the whole diff' l diff -- "$long"
ok 'diff --exit-code -- <name too long> finds no change' l diff --exit-code -- "$long"
finish
