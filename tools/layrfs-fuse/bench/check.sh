#!/bin/bash
# Correctness, crash and tamper checks of the FUSE view (`layr fs view`, docs/layrfs-fuse.md).
# Run as the agent user after setup.sh. Needs: B, LF (the bench helpers), LAYR (a layr built with
# the feature layrfs-fuse; setup.sh installs a copy for the user layr at /home/layr/layr).
#   1. The same edits in the view and in a native folder give the same tree id.
#   2. kill -9 of the server between edits: a restart replays the journal; the save is still equal.
#   3. git works in the view (status, add, commit).
#   4. The agent cannot read or change the store; another user cannot use the mount or the socket.
#   5. `layr tool rg|find` in the view (served from the view's index) equals the real tools on the
#      mount: at the start, at once after edits, renames and removals, after a save, a restart and a reset.
#   6. git status with the fsmonitor hook equals git status without it, after the same steps.
#   7. A write is visible at once through the mount, after a save, and a reset shows the head at once.
set -uo pipefail
: "${B:?}" "${LF:?}" "${LAYR:?}"
LFL=/home/layr/layrfs-fuse
VL=/home/layr/layr
me=$(id -u); mygid=$(id -g)
PASS=0 FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok   $*"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL $*"; }
# The service user reads the source trees: they live outside the agent's home (SRCROOT, mode 0755).
W=${SRCROOT:-/home/layrfs-src}/check-$$
mkdir -p "$W"

# The source tree.
S=$W/src
mkdir -p "$S/dir1" "$S/dir2/sub" "$S/dir3"
printf 'a\n' > "$S/a.txt"; printf 'd\n' > "$S/d.txt"; printf '#!/bin/sh\necho run\n' > "$S/run.sh"
head -c 1048576 /dev/urandom > "$S/big.bin"
printf 'b\n' > "$S/dir1/b.txt"; printf 'c\n' > "$S/dir1/c.txt"
printf 'e\n' > "$S/dir2/e.txt"; printf 'f\n' > "$S/dir2/sub/f.txt"; printf 'g\n' > "$S/dir3/g.txt"
ln -s a.txt "$S/link"
sudo -u layr "$LFL" import "$S" /home/layr/store/objects "ck-$$" >/dev/null
"$LF" import "$S" "$W/objects" "ck-$$" >/dev/null

e1() {
    (cd "$1" &&
        printf 'changed\n' > a.txt && printf 'more\n' >> dir1/b.txt && mkdir -p new/deep && printf 'n\n' > new/deep/n.txt &&
        rm dir1/c.txt && printf 'e2\n' >> dir2/e.txt && mv dir2 dir2-moved && mv d.txt dir1/d-moved.txt && chmod +x run.sh &&
        ln -s a.txt link2 && truncate -s 3 big.bin && touch -d '2020-01-01 00:00:00' a.txt && printf 'over\n' > over.txt &&
        mv over.txt dir1/b.txt)
}
e2() {
    (cd "$1" && rm -r dir3 && printf 'x\n' > dir2-moved/x.txt && mv new new2 && mkdir empty && printf 'y\n' > empty2.txt &&
        rm empty2.txt)
}

# The expected tree: the same edits in a native folder.
N=$W/native
"$LF" checkout "$W/objects" "ck-$$" "$N" >/dev/null
e1 "$N" && e2 "$N"
want=$("$LF" import "$N" "$W/objects" "ck-native-$$" | grep -o '"tree":"[0-9a-f]*"')

start() { # start <line>
    mkdir -p "$W/caches/$1/target"
    sudo -u layr sh -c "{ mkdir -p /home/layr/lines/$1 2>/dev/null || true; } && exec $VL fs view start --objects /home/layr/store/objects \
        --state ${2:-ck-$$} --line /home/layr/store/lines/$1 --mnt /home/layr/lines/$1 --sock /home/layr/run/$1.sock \
        --cache target=$W/caches/$1/target --allow-other --allow $me --owner $me ${LAYRFS_EXTRA:-}"
}
ctl() { "$LAYR" fs view ctl "/home/layr/run/$1.sock" "${@:2}"; }
kill9() { sudo -u layr pkill -9 -f "f[s] view (serve|start).*--line /home/layr/store/lines/$1 "; sleep 0.3; }
stop() {
    ctl "$1" stop >/dev/null
    sudo -u layr sh -c "while mountpoint -q /home/layr/lines/$1; do sleep 0.01; done; rm -rf /home/layr/store/lines/$1; rmdir /home/layr/lines/$1"
}

# 1. Same edits in the view.
L=ck1-$$; V=/home/layr/lines/$L
start "$L"
[ "$(readlink "$V/target")" = "$W/caches/$L/target" ] && ok "target is a link to the native cache folder" || bad "target link"
e1 "$V" && e2 "$V" && ok "edits in the view" || bad "edits in the view"
diff -r --no-dereference "$N" "$V" >/dev/null 2>&1 || diff -r --no-dereference -x target "$N" "$V" >/dev/null && ok "view content equals the native folder" || bad "content differs: $(diff -r --no-dereference -x target "$N" "$V" 2>&1 | head -3)"
ctl "$L" status > "$W/status"
grep -q '^D dir3/g.txt$' "$W/status" && grep -q '^A new2/deep/n.txt$' "$W/status" && grep -q '^M a.txt$' "$W/status" &&
    ! grep -q target "$W/status" && ok "status lists A, M, D and leaves out the cache" || bad "status: $(head -5 "$W/status")"
got=$(ctl "$L" save "ck1-save-$$" | grep -o '"tree":"[0-9a-f]*"')
[ -n "$want" ] && [ "$got" = "$want" ] && ok "save gives the tree of the native folder ($want)" || bad "save tree $got, want $want"
ctl "$L" status > "$W/status2"
grep -q '^# 0 changes' "$W/status2" && ok "status is clean after the save" || bad "status after save: $(head -3 "$W/status2" | tr '\n' ' ')"
printf 'after\n' >> "$V/a.txt"
ctl "$L" status | grep -q '^M a.txt$' && ok "an edit after the save shows" || bad "edit after save"
stop "$L"

# 2. A crash between e1 and e2.
L=ck2-$$; V=/home/layr/lines/$L
start "$L"
e1 "$V"
kill9 "$L"
# statfs always asks the server (stat and ls may be answered from the kernel's caches).
{ stat -f "$V" 2>&1 || true; } | grep -q 'not connected' && ok "the mount gives ENOTCONN after kill -9" || bad "the mount after kill -9: $(stat -f "$V" 2>&1 | head -1)"
start "$L"
grep -q 'replayed' "/home/layr/store/lines/$L/server.log" 2>/dev/null || sudo -u layr grep -q replayed "/home/layr/store/lines/$L/server.log" && ok "the restart replayed the journal" || bad "no replay"
e2 "$V" && ok "edits after the restart" || bad "edits after restart"
got=$(ctl "$L" save "ck2-save-$$" | grep -o '"tree":"[0-9a-f]*"')
[ "$got" = "$want" ] && ok "save after a crash gives the same tree" || bad "save after crash $got, want $want"
stop "$L"

# 3. git in the view.
G=$W/gitsrc
mkdir -p "$G" && (cd "$G" && git init -q && printf 'x\n' > x && git add x && git -c user.name=t -c user.email=t@t commit -qm x)
sudo -u layr "$LFL" import "$G" /home/layr/store/objects "ckgit-$$" >/dev/null
L=ck3-$$; V=/home/layr/lines/$L
mkdir -p "$W/caches/$L"
start "$L" "ckgit-$$"
(cd "$V" && printf 'y\n' > y && git add y && git -c user.name=t -c user.email=t@t commit -qm y && [ -z "$(git status --porcelain | grep -v '^?? target$')" ] &&
    git log --oneline | wc -l | grep -qx 2) > "$W/git.out" 2>&1 && ok "git add, commit and status in the view" ||
    bad "git in the view: $(cat "$W/git.out"; cd "$V" && git status --porcelain 2>&1 | head -3; git log --oneline 2>&1 | head -3)"
stop "$L"

# 4. Tamper model.
L=ck4-$$; V=/home/layr/lines/$L
start "$L"
ls /home/layr/store >/dev/null 2>&1 && bad "the agent can list the store" || ok "the agent cannot list the store (0700, user layr)"
rm -f "/home/layr/store/objects/states/ck-$$" 2>/dev/null; sudo -u layr test -f "/home/layr/store/objects/states/ck-$$" && ok "the agent cannot remove a saved state" || bad "state removed"
sudo -u nobody ls "$V" >/dev/null 2>&1 && bad "another user can read the mount" || ok "another user cannot read the mount"
{ sudo -u nobody "$VL" fs view ctl "/home/layr/run/$L.sock" save "evil-$$" 2>&1 || true; } | grep -q 'permission denied' && ok "another user cannot ask for a save" || bad "socket open to others"
stat -c '%U' "$V/a.txt" | grep -qx "$(id -un)" && ok "the agent owns the files it sees" || bad "owner in the view"
sudo -u layr stat -c '%U' "$(sudo -u layr ls -d /home/layr/store/lines/$L/upper)" | grep -qx layr && ok "the upper folder belongs to layr" || bad "upper owner"
stop "$L"

# 5 to 7: a git tree with ignore rules, hidden files and text to search.
T=$W/tools
mkdir -p "$T/src/deep" "$T/docs" "$T/ignored" "$T/.hidden"
printf 'ignored/\n*.log\n' > "$T/.gitignore"
printf 'fn main() {} // TODO one\n' > "$T/src/main.rs"; printf 'TODO two\nline\n' > "$T/src/deep/a.txt"
printf 'no match\n' > "$T/docs/readme.md"; printf 'TODO ignored\n' > "$T/ignored/x.txt"; printf 'TODO log\n' > "$T/run.log"
printf 'TODO hidden\n' > "$T/.hidden/h.txt"; ln -s src/main.rs "$T/main-link"
(cd "$T" && git init -q && git add -A && git -c user.name=t -c user.email=t@t commit -qm base)
sudo -u layr "$LFL" import "$T" /home/layr/store/objects "cktools-$$" >/dev/null
L=ck5-$$; V=/home/layr/lines/$L
start "$L" "cktools-$$"
for i in $(seq 100); do "$LAYR" fs view ctl "/home/layr/run/$L.sock" stats | grep -q '"index_entries":[0-9]' && break; sleep 0.1; done
real_rg=$(command -v rg)
same_tools() { # same_tools <step>: the served answers equal the real tools on the mount
    local q out want got fails=0
    set -f
    for q in "-l TODO" "-n TODO" "--files" "-c TODO src" "-i todo docs src" "-l TODO -g !*.rs" "-n -w line"; do
        want=$(cd "$V" && $real_rg --sort path $q 2>&1 </dev/null; echo "rc=$?")
        got=$(cd "$V" && LAYR_FS_STRICT=1 "$LAYR" tool rg $q 2>&1 </dev/null; echo "rc=$?")
        [ "$want" = "$got" ] || { fails=$((fails + 1)); echo "  rg $q: want [$want] got [$got]"; }
    done
    for q in ". -type f" ". -name *.txt" "src -type d" ". -maxdepth 1" ". -path ./src/*"; do
        want=$(cd "$V" && find $q 2>&1 </dev/null | LC_ALL=C sort)
        got=$(cd "$V" && LAYR_FS_STRICT=1 "$LAYR" tool find $q 2>&1 </dev/null | LC_ALL=C sort)
        [ "$want" = "$got" ] || { fails=$((fails + 1)); echo "  find $q: want [$want] got [$got]"; }
    done
    set +f
    [ "$fails" = 0 ] && ok "layr tool rg|find equal the real tools: $1" || bad "layr tool rg|find differ: $1"
}
(cd "$V" && git config core.fsmonitor "$LAYR fs view fsmonitor" && git config core.untrackedCache true)
same_git() { # same_git <step>: git status with the hook equals git status without it
    local want got rc1 rc2
    got=$(cd "$V" && git status --porcelain=v1 -uall 2>&1); rc1=$?
    cp "$V/.git/index" "$W/index-copy"
    want=$(cd "$V" && GIT_INDEX_FILE="$W/index-copy" git -c core.fsmonitor= -c core.untrackedCache=false status --porcelain=v1 -uall 2>&1); rc2=$?
    [ "$rc1" = 0 ] && [ "$rc2" = 0 ] && [ "$want" = "$got" ] && ok "git status with fsmonitor equals the plain run: $1" || bad "git status differs: $1: want [$want] got [$got]"
}
same_tools "start"; same_git "start"; same_git "start, again (token)"
printf 'TODO new\n' > "$V/src/new.txt"; printf 'TODO more\n' >> "$V/docs/readme.md"
same_tools "after writes"; same_git "after writes"
mv "$V/src/deep" "$V/src/deeper"; mv "$V/docs/readme.md" "$V/docs/README.md"
same_tools "after renames"; same_git "after renames"
rm "$V/src/main.rs"; rm -r "$V/.hidden"; chmod +x "$V/src/new.txt"
same_tools "after removals and a chmod"; same_git "after removals and a chmod"
mkdir "$V/fresh"; printf 'TODO fresh\n' > "$V/fresh/f.txt"; printf 'x\n' > "$V/fresh/ignored.log"
same_tools "after a new folder"; same_git "after a new folder"
# The served search reads the backing files, never the mount: no open through FUSE.
opens() { ctl "$L" stats | grep -o '"open":[0-9]*' | cut -d: -f2; }
o1=$(opens); (cd "$V" && LAYR_FS_STRICT=1 "$LAYR" tool rg -l TODO </dev/null >/dev/null); o2=$(opens)
[ "$o1" = "$o2" ] && ok "layr tool rg opens no file through the mount" || bad "layr tool rg opened $((o2 - o1)) files through the mount"
# 7. Visibility at once.
printf 'TODO visible\n' > "$V/visible.txt"
[ "$(cat "$V/visible.txt")" = "TODO visible" ] && (cd "$V" && LAYR_FS_STRICT=1 "$LAYR" tool rg -l visible </dev/null | grep -qx visible.txt) &&
    ok "a write is visible at once through the mount and to layr tool rg" || bad "a write is not visible at once"
ctl "$L" save "ck5-save-$$" >/dev/null
[ "$(cat "$V/visible.txt")" = "TODO visible" ] && [ "$(cat "$V/docs/README.md")" = "$(printf 'no match\nTODO more')" ] &&
    ok "content is the same at once after a save" || bad "content after a save"
same_tools "after a save"; same_git "after a save"
printf 'TODO after save\n' >> "$V/visible.txt"
[ "$(tail -1 "$V/visible.txt")" = "TODO after save" ] && ok "a write after a save is visible at once" || bad "write after save"
kill9 "$L"; start "$L" "cktools-$$"
for i in $(seq 100); do "$LAYR" fs view ctl "/home/layr/run/$L.sock" stats | grep -q '"index_entries":[0-9]' && break; sleep 0.1; done
same_tools "after kill -9 and a restart"; same_git "after kill -9 and a restart"
mv "$V/fresh" "$V/fresh2"; printf 'TODO late\n' > "$V/fresh2/late.txt"
same_tools "after the restart, more edits"; same_git "after the restart, more edits"
# A reset is a change the server makes, not the kernel: the cached names and pages must go.
cat "$V/visible.txt" >/dev/null; ls "$V/fresh2" >/dev/null
ctl "$L" reset >/dev/null
[ ! -e "$V/fresh2" ] && [ -e "$V/fresh/f.txt" ] && [ "$(cat "$V/visible.txt")" = "TODO visible" ] &&
    ok "a reset shows the head at once (names and pages invalidated)" || bad "after reset: $(ls "$V" | tr '\n' ' ')"
for i in $(seq 100); do "$LAYR" fs view ctl "/home/layr/run/$L.sock" stats | grep -q '"index_entries":[0-9]' && break; sleep 0.1; done
same_tools "after a reset"; same_git "after a reset"
ctl "$L" stats > "$W/stats"; grep -q '"fsmonitor_listed":[1-9]' "$W/stats" &&
    ok "git used the hook's change lists ($(grep -o '"fsmonitor[a-z_]*":[0-9]*' "$W/stats" | tr '\n' ' '))" || bad "the hook gave no change list: $(cat "$W/stats")"
stop "$L"

echo "check: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
