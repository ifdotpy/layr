#!/bin/bash
# ci: manual-fskit
# Virtual lines (docs/views.md): a line whose folder is a LayrFS mount of an upper folder over the
# tree of its start state. Needs macOS 27 and the signed LayrFS extension, installed and enabled
# (tools/layrfs/README.md). CI runners cannot load it, so no CI job selects this test; on a machine
# without the extension it skips. Run: LAYR_APFS_TEST_ROOT=$TMPDIR tests/run.sh t199-virtual-lines
# The LayrFS tool server (`layr tool rg|find`) serves a virtual line from its lower tree and upper
# folder, not through the mount; its output must equal the real tools on the mount and on a folder line.
if [ "$(uname -s)" != Darwin ]; then
    echo "skip: virtual lines need macOS"
    exit 0
fi
if ! pluginkit -m -p com.apple.fskit.fsmodule 2>/dev/null | grep -q dev.layr.fshost.fs; then
    echo "skip: the LayrFS file system extension is not installed (tools/layrfs/README.md)"
    exit 0
fi
. "$(dirname "$0")/lib.sh"
export LAYR_EXPERIMENTAL_VIEWS=1

# Unmount every LayrFS mount of this test before the cleanup deletes the test folder.
unmount_views() {
    local mp
    for mp in $(mount | awk -v t="$T" '/\(layrfs,/ && index($3, t) == 1 { print $3 }'); do
        umount "$mp" 2>/dev/null || umount -f "$mp"
    done
}
command -v rg >/dev/null || { echo "FATAL: rg (ripgrep) is not on PATH" >&2; exit 1; }
export LAYR_FS_SOCKET=$T/fs.sock LAYR_FS_STRICT=1 LAYR_FS_NO_WATCH=
unset LAYR_FS_OFF LAYR_TOOL_ACTIVE RIPGREP_CONFIG_PATH
trap '"$LAYR" fs stop >/dev/null 2>&1; unmount_views; cleanup' EXIT

src=$T/src
mkdir -p "$src/dir/sub" "$src/src"
seq 1 20 > "$src/notes.txt"
printf 'fn main() {}\n' > "$src/src/main.rs"
echo a > "$src/dir/a.txt"
echo c > "$src/dir/c.txt"
echo keep > "$src/dir/keep.txt"
echo trunc > "$src/dir/trunc.txt"
echo b > "$src/dir/sub/b.txt"
ln -s notes.txt "$src/link"
printf 'target/\n*.log\n' > "$src/.gitignore"
mkdir -p "$src/vendor" "$src/.hidden" "$src/src/deep"
printf 'fn main() {\n    // TODO: one\n}\n' > "$src/src/lib.rs"
printf 'fn deep() {} // TODO deep\n' > "$src/src/deep/x.rs"
printf 'fn vendored() {} // TODO vendor\n' > "$src/vendor/v.rs"
printf 'fn hidden() {} // TODO hidden\n' > "$src/.hidden/h.rs"
printf 'TODO generated\n' > "$src/src/out.gen"
printf 'vendor/\n' > "$src/.ignore"
printf '*.gen\n' > "$src/.rgignore"
l init demo --from "$src" > /dev/null
P=$LAYR_ROOT/projects/demo
cd "$P/lines/main"
l line new nat > /dev/null

# The switch is required.
no "virtual line needs LAYR_EXPERIMENTAL_VIEWS=1" env LAYR_EXPERIMENTAL_VIEWS= "$LAYR" line new off --view virtual
ok "line new --view virtual" l line new virt --view virtual
V=$P/lines/virt
has "the line folder is a LayrFS mount" "$V (layrfs" "$(mount)"
eq "a file reads through the mount" "$(cat "$P/lines/nat/notes.txt")" "$(cat "$V/notes.txt")"
eq "a listing through the mount" "$(cd "$P/lines/nat" && ls -A | grep -v '^.git$')" "$(cd "$V" && ls -A)"
base_tree=$(sed -n 's/^lower=//p' "$P"/meta/vlines/*/layrfs-view.conf)
has "the lower tree is a complete state tree" "$P/states/" "$base_tree"
ok "the lower tree exists" test -f "$base_tree/notes.txt"
ok "the lower tree stays sealed" test -n "$(ls -lO "$base_tree/notes.txt" | grep uchg)"

# `layr tool rg|find` in the virtual line against the real tools on the mount and on the folder line.
N=$P/lines/nat
eq "the tool server indexes the virtual line" "ready" "$(cd "$V" && "$LAYR" fs warm --wait)"
eq "the tool server indexes the folder line" "ready" "$(cd "$N" && "$LAYR" fs warm --wait)"
has "the server reads the lower tree and the upper folder" "$V" "$("$LAYR" fs status)"
tools_same() { # tools_same <label>: every call in the virtual line equals the mount and the folder line
    local label=$1 args want got nat
    set -f
    for args in "fn" "--files" "-l TODO" "-n TODO" "-c fn" "-i todo src" "--files-without-match TODO" "-t rust fn" "-g !src/deep TODO"; do
        # shellcheck disable=SC2086
        want=$(cd "$V" && rg --sort path $args </dev/null 2>&1; echo "exit $?")
        # shellcheck disable=SC2086
        got=$(cd "$V" && "$LAYR" tool rg $args </dev/null 2>&1; echo "exit $?")
        # shellcheck disable=SC2086
        nat=$(cd "$N" && rg --sort path $args </dev/null 2>&1; echo "exit $?")
        eq "$label: layr tool rg $args equals rg on the mount" "$want" "$got"
        eq "$label: layr tool rg $args equals rg on the folder line" "$nat" "$got"
    done
    for args in "." ". -type f" ". -type d" ". -name *.rs" "src -maxdepth 1"; do
        # shellcheck disable=SC2086
        want=$(cd "$V" && find $args </dev/null 2>&1 | LC_ALL=C sort)
        # shellcheck disable=SC2086
        got=$(cd "$V" && "$LAYR" tool find $args </dev/null 2>&1 | LC_ALL=C sort)
        # shellcheck disable=SC2086
        nat=$(cd "$N" && find $args </dev/null 2>&1 | grep -v -e '^\./\.git$' -e '^\./\.git/' | LC_ALL=C sort)
        eq "$label: layr tool find $args equals find on the mount" "$want" "$got"
        eq "$label: layr tool find $args equals find on the folder line" "$nat" "$got"
    done
    set +f
}
tools_same "start"

# The same edits in the virtual line and in a folder line.
edits() {
    echo "line 21" >> notes.txt
    echo new > new.txt
    mkdir newdir && echo n > newdir/n.txt
    rm dir/a.txt
    mv dir/c.txt moved.txt
    mv dir/sub sub2
    chmod 755 src/main.rs
    ln -s notes.txt newlink
    : > dir/trunc.txt
    echo x > debug.log
    mkdir -p target && echo bin > target/out.bin
}
(cd "$P/lines/nat" && edits)
ok "edits through the mount" bash -c "cd '$V' && $(declare -f edits); edits"
ok "the base tree did not change" test "$(cat "$base_tree/notes.txt" | wc -l | tr -d ' ')" = 20
eq "the lower folder was moved with mv (EXDEV fallback)" "b" "$(cat "$V/sub2/b.txt")"
ok "the upper folder holds the changes" test -f "$P"/meta/vlines/*/upper/new.txt
tools_same "after edits"

st=$(cd "$V" && l status)
has "status: modified" "modified:   notes.txt" "$st"
has "status: new file" "new file:   new.txt" "$st"
has "status: deleted" "deleted:    dir/a.txt" "$st"
has "status: rename" "renamed:    dir/c.txt -> moved.txt" "$st"
has "status: moved folder" "sub2/b.txt" "$st"
case "$st" in *debug.log*|*target/*) FAIL=$((FAIL + 1)); echo "FAIL status leaves ignored files out" ;; *) PASS=$((PASS + 1)); echo "PASS status leaves ignored files out" ;; esac
d=$(cd "$V" && l diff)
has "diff shows the new line" "+line 21" "$d"
has "diff shows the mode change" "new mode 100755" "$d"
no "layr add is refused in a virtual line" bash -c "cd '$V' && '$LAYR' add new.txt"
no "layr save without -m is refused" bash -c "cd '$V' && '$LAYR' save"

ok "save -m in the virtual line" bash -c "cd '$V' && '$LAYR' save -m edits"
(cd "$P/lines/nat" && l add new.txt newdir moved.txt sub2 newlink > /dev/null && l save -a -m edits > /dev/null)
nat1=$(l rev-parse nat)
virt1=$(l rev-parse virt)
ok "the virtual save made a new head" test "$virt1" != "$(l rev-parse main)"
ok "the saved state equals the folder line's state" l diff --exit-code "$nat1" "$virt1"
no "the compare sees a difference (main differs)" l diff --exit-code main "$virt1"
has "status is clean after save" "nothing to save" "$(cd "$V" && l status)"

# A second round: change a saved new file, delete another saved new file (no whiteout).
round2() {
    echo again >> new.txt
    rm newdir/n.txt
    rmdir newdir
    echo trunc2 > dir/trunc.txt
}
(cd "$P/lines/nat" && round2 && l save -a -m round2 > /dev/null)
ok "second edits through the mount" bash -c "cd '$V' && $(declare -f round2); round2"
st=$(cd "$V" && l status --short)
has "status shows the delete of a saved new file" "D  newdir/n.txt" "$st"
ok "second save" bash -c "cd '$V' && '$LAYR' save -m round2"
ok "the second state equals the folder line's state" l diff --exit-code "$(l rev-parse nat)" "$(l rev-parse virt)"

# Changes right before each call: a new file, a delete, a rename, a new folder, a changed ignore file.
both() { (cd "$N" && eval "$1") && (cd "$V" && eval "$1"); }
both 'printf "fn fresh() {} // TODO fresh\n" > src/fresh.rs'
tools_same "a new file just before"
both 'rm src/deep/x.rs'
tools_same "a delete just before"
both 'mv src/lib.rs src/lib2.rs'
tools_same "a rename just before"
both 'mkdir -p gen/a && printf "fn g() {} // TODO g\n" > gen/a/g.rs'
tools_same "a new folder just before"
both 'printf "vendor/\ngen/\n" > .ignore'
tools_same "a changed ignore file just before"
both 'rm .rgignore'
tools_same "a removed ignore file just before"
both 'echo "// TODO edited" >> src/main.rs'
tools_same "a changed lower file just before"

# gc keeps the lower tree of a virtual line.
ok "gc with a virtual line" l gc
ok "the lower tree survives gc" test -f "$base_tree/notes.txt"

# No folder, then the mount again: the layer stays.
echo pending > "$V/pending.txt"
ok "line show --none" l line show virt --none
no "no mount after --none" bash -c "mount | grep -q '$V (layrfs'"
has "status of a line without a folder reads its layer" "new file:   pending.txt" "$(l --project demo --line virt status)"
ok "line show --virtual" l line show virt --virtual
eq "the unsaved file is back" "pending" "$(cat "$V/pending.txt")"

# A folder with the merged content: the line becomes a folder line.
ok "line show --folder" l line show virt --folder
no "no mount after --folder" bash -c "mount | grep -q '$V (layrfs'"
ok "the folder is a plain directory" test -d "$V/sub2"
eq "the folder has the unsaved file" "pending" "$(cat "$V/pending.txt")"
eq "the folder content equals the folder line plus the unsaved file" "Only in $V: pending.txt" \
    "$(diff -r -x .git -x target -x debug.log "$P/lines/nat" "$V" 2>&1)"
has "the converted line's status shows the unsaved file" "pending.txt" "$(cd "$V" && l status --short)"
ok "no view is left" test -z "$(ls "$P/meta/vlines" 2>/dev/null)"

# Delete a virtual line with unsaved work: the work goes into its final state.
ok "a second virtual line" l line new v2 --view virtual
echo unsaved > "$P/lines/v2/unsaved.txt"
ok "line delete -D of a virtual line" l line delete -D v2
no "the mount is gone" bash -c "mount | grep -q '$P/lines/v2 (layrfs'"
ok "the line folder is gone" test ! -e "$P/lines/v2"
ok "the view folder is gone" test -z "$(ls "$P/meta/vlines" 2>/dev/null)"

finish
