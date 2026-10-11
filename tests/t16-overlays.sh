#!/bin/bash
# Overlays (docs/overlays.md): files with their own readers and history, shown in the lines of
# their readers and nowhere else. A private loader of one user, a project key for a group,
# follow mode with branch and merge, the commit split by layer.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
getent group layr-sap >/dev/null || groupadd layr-sap
for u in layr-o1 layr-o2 layr-o3; do id $u >/dev/null 2>&1 || useradd -m $u; done
usermod -aG layr-sap layr-o2
gpasswd -d layr-o3 layr-sap >/dev/null 2>&1 || true
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 3600 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
L=$T/layr
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
# A command of a user in a line (the line folder belongs to its owner only).
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=sap LAYR_LINE=$line $L "$@"; }
# A shell command of a user inside a line folder.
sh_in() { local u=$1 line=$2; shift 2; as $u sh -c "cd $LAYR_ROOT/projects/sap/lines/$line && $*"; }
P=$LAYR_ROOT/projects/sap/lines
SECRET=kc-secret-7f3a
KEY=zk-secret-91b2

sample $T/src
printf 'dotenv_if_exists ./.zulip.env\n' > $T/src/.envrc
printf '.zulip.env\n' >> $T/src/.gitignore
l init sap --from $T/src --owner layr-o1 -q >/dev/null
l --project sap access grant layr-o2 writer
l --project sap access grant layr-o3 writer
cd /tmp || exit 1

# 1. The private loader of the owner of main: a file of main's folder goes into an overlay that
# only its owner reads. main is protected; a commit that changes only overlays is the owner's.
sh_in layr-o1 main "echo 'export KEYCHAIN=$SECRET' > .codex-keychain.envrc"
ok "the owner makes an overlay" in_line layr-o1 main overlay create keychain -q
eq "it lists with its mode and reader" "keychain	shared	owner layr-o1	readers layr-o1" "$(in_line layr-o1 main overlay list | sed 's/^ *//')"
out=$(in_line layr-o1 main overlay move keychain .codex-keychain.envrc)
has "move names the next commit" "move to overlay keychain at the next commit: .codex-keychain.envrc" "$out"
has "status shows the move" "keychain: .codex-keychain.envrc" "$(in_line layr-o1 main status)"
out=$(in_line layr-o1 main commit -m "keychain loader in an overlay"); rc=$?
eq "an overlay-only commit in protected main" "0" "$rc"
has "it names the overlay" "[main overlay keychain " "$out"
eq "main did not move" "1" "$(in_line layr-o1 main log --oneline | wc -l)"
eq "the owner's folder keeps the file" "export KEYCHAIN=$SECRET" "$(sh_in layr-o1 main cat .codex-keychain.envrc)"
eq "and its status is clean" "" "$(in_line layr-o1 main status --porcelain)"
eq "the overlay has the file" "export KEYCHAIN=$SECRET" "$(in_line layr-o1 main overlay show keychain HEAD:.codex-keychain.envrc)"
no "the shared set does not" in_line layr-o1 main show HEAD:.codex-keychain.envrc

# 2. Another user's line from main has no trace of it.
ok "a non-reader makes a line from main" in_line layr-o3 main branch -q t3 main
no "its folder lacks the file" test -e $P/t3/.codex-keychain.envrc
eq "its status is clean" "" "$(in_line layr-o3 t3 status --porcelain)"
eq "it shows no overlays" "line t3 shows no overlays" "$(in_line layr-o3 t3 overlay use)"
eq "diff against main is empty" "" "$(in_line layr-o3 t3 diff main)"
no "show of main lacks it" in_line layr-o3 t3 show main:.codex-keychain.envrc
eq "grep finds nothing" "" "$(in_line layr-o3 t3 grep -l $SECRET main)"
eq "ls-files lacks it" "" "$(in_line layr-o3 t3 ls-files | grep keychain)"
as layr-o3 mkdir -p /tmp/layr-o3-exp-$$
in_line layr-o3 t3 export -q main /tmp/layr-o3-exp-$$/e
no "an export of main lacks it" test -e /tmp/layr-o3-exp-$$/e/.codex-keychain.envrc
rm -rf /tmp/layr-o3-exp-$$
# The owner's working content is private to readers.
has "a non-reader cannot read main's working content" "shows overlays that you do not read" "$(in_line layr-o3 main status 2>&1)"
no "nor its index" in_line layr-o3 main diff --cached
# The overlay's history: readers only, paths included.
has "a non-reader cannot read the overlay's log" "may not read project sap%keychain" "$(in_line layr-o3 t3 overlay log keychain 2>&1)"
no "nor show its files" in_line layr-o3 t3 overlay show keychain user-$(id -u layr-o1):.codex-keychain.envrc
no "nor list its paths" in_line layr-o3 t3 overlay ls-files keychain
no "nor through its project name" as layr-o3 $L --project sap%keychain --line user-$(id -u layr-o1) log
no "an overlay takes reading commands only" as layr-o1 $L --project sap%keychain --line user-$(id -u layr-o1) access grant layr-o3 reader
no "a non-reader cannot use the overlay" in_line layr-o3 t3 overlay use keychain
no "a project name with % is refused" l init 'sap%keychain' --from $T/src -q
# Automatic states of main leave the overlay files out.
in_line layr-o1 main save -q --force
no "main's automatic state lacks the file" in_line layr-o1 main show HEAD@{0}:.codex-keychain.envrc
no "for other users too" in_line layr-o3 t3 show main@{0}:.codex-keychain.envrc
# A copy of the file under another name (an editor's backup) stays out of them too.
sh_in layr-o1 main "cp .codex-keychain.envrc .codex-keychain.envrc~ && echo other > other.txt"
in_line layr-o1 main save -q
no "a copy of the file is not in the automatic state" in_line layr-o1 main show HEAD@{0}:.codex-keychain.envrc~
ok "another new file is" in_line layr-o1 main show HEAD@{0}:other.txt
sh_in layr-o1 main "rm .codex-keychain.envrc~ other.txt"

# 3. The owner changes the file and commits: only the overlay's history moves.
sh_in layr-o1 main "echo 'export KEYCHAIN2=$SECRET-2' >> .codex-keychain.envrc"
eq "status shows the change" " M .codex-keychain.envrc" "$(in_line layr-o1 main status --porcelain)"
has "diff shows it to the owner" "+export KEYCHAIN2=$SECRET-2" "$(in_line layr-o1 main diff)"
ok "commit -a takes it" in_line layr-o1 main commit -qam "second key"
eq "the overlay has two commits after its first state" "second key|keychain loader in an overlay|overlay keychain" "$(in_line layr-o1 main overlay log keychain --format=%s | paste -sd'|')"
has "overlay show -p shows the change" "+export KEYCHAIN2=$SECRET-2" "$(in_line layr-o1 main overlay show keychain)"
eq "main still has one commit" "1" "$(in_line layr-o1 main log --oneline | wc -l)"
eq "no state of the shared set has the path" "0" "$(in_line layr-o1 main log -p --format= | grep -c keychain)"

# 4. Push, backups: the shared set only.
git init -q --bare -b main $T/r.git; chown -R layr-o1 $T/r.git
l --project sap remote add origin $T/r.git
ok "push main" in_line layr-o1 main push -q origin main
eq "the git remote has no trace of the path" "0" "$(git --git-dir=$T/r.git log --all -p --format=%B | grep -c 'keychain\|KEYCHAIN')"
eq "nor of the secret" "0" "$(git --git-dir=$T/r.git log --all -p | grep -c $SECRET)"
l --project sap backup -q $T/bk
n=0; for f in $(find $T/bk -type f); do if (zstd -dcq $f 2>/dev/null || cat $f) | grep -qa "$SECRET"; then n=$((n + 1)); fi; done
eq "the backup has no trace of the secret" "0" "$n"
eq "nor of the path" "0" "$(for f in $(find $T/bk -type f); do zstd -dcq $f 2>/dev/null || cat $f; done | grep -ca codex-keychain)"
new_fs $T/b 4G
ok "the backup restores" env LAYR_ROOT=$T/b/store $LAYR backup restore -q $T/bk
no "the restored main has no private file" test -e $T/b/store/projects/sap/lines/main/.codex-keychain.envrc
eq "and is clean" "" "$(env LAYR_ROOT=$T/b/store $LAYR --project sap --line main status --porcelain)"
eq "the restored store has no trace of the secret" "" "$(grep -rla $SECRET $T/b/store 2>/dev/null)"
ok "a new line from the restored main" env LAYR_ROOT=$T/b/store $LAYR --project sap branch -q rb main
umount $T/b

# 5. The owner's other lines show it (shared mode: one overlay line for all lines of a user).
ok "the owner makes a line from main" in_line layr-o1 main branch -q o1t main
eq "it has the file" "export KEYCHAIN=$SECRET" "$(sh_in layr-o1 o1t head -1 .codex-keychain.envrc)"
eq "its status is clean" "" "$(in_line layr-o1 o1t status --porcelain)"
sh_in layr-o1 o1t "echo w >> notes.txt"
eq "diff HEAD shows the changed file only, not the overlay files" "notes.txt" "$(in_line layr-o1 o1t diff --name-only HEAD)"
sh_in layr-o1 o1t "$L restore notes.txt"
# A moved private file stays private; a new file goes to the shared set.
sh_in layr-o1 o1t "mv .codex-keychain.envrc .kc-renamed && echo pub > public.txt && $L add -A && $L commit -qm 'rename and add'"
no "the renamed file is not in the shared set" in_line layr-o1 o1t show HEAD:.kc-renamed
ok "the new file is" in_line layr-o1 o1t show HEAD:public.txt
ok "the overlay has the new name" in_line layr-o1 o1t overlay show keychain HEAD:.kc-renamed
sh_in layr-o1 o1t "echo w >> notes.txt"
eq "diff against another state leaves the overlay files out" "notes.txt|public.txt" "$(in_line layr-o1 o1t diff --name-only main | paste -sd'|')"
sh_in layr-o1 o1t "$L restore notes.txt"
# main shows the overlay state it took; a commit there of an older state is refused.
sh_in layr-o1 main "echo extra >> .codex-keychain.envrc"
has "a stale overlay refuses the commit" "changed in another line" "$(in_line layr-o1 main commit -qam stale 2>&1)"
sh_in layr-o1 main "sed -i '\$d' .codex-keychain.envrc"
eq "a refused commit -a leaves the index" "" "$(in_line layr-o1 main status --porcelain)"
ok "overlay use takes the newest state" in_line layr-o1 main overlay use -q keychain
ok "the renamed file comes to main" test -e $P/main/.kc-renamed
no "the old name goes" test -e $P/main/.codex-keychain.envrc
eq "main is clean" "" "$(in_line layr-o1 main status --porcelain)"

# 6. A merge into main keeps the overlay in place.
in_line layr-o2 main branch -q t2 main
sh_in layr-o2 t2 "echo work >> notes.txt && $L commit -qam 'o2 work'"
S2=$(in_line layr-o2 t2 rev-parse HEAD)
ok "the owner merges a reviewed line into main" in_line layr-o1 main merge -q t2 --expect $S2
eq "main has the change" "work" "$(sh_in layr-o1 main tail -1 notes.txt)"
ok "and its overlay file" test -e $P/main/.kc-renamed
eq "and is clean" "" "$(in_line layr-o1 main status --porcelain)"
eq "fsck is clean" "0 problem(s)" "$(l --project sap fsck | tail -1)"

# 7. A project key for a group: readers are users and Unix groups.
sh_in layr-o1 main "echo 'ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY' > .zulip.env"
eq "the key is ignored in main" "!! .zulip.env" "$(in_line layr-o1 main status --porcelain --ignored | grep zulip)"
ok "the owner makes an overlay for a group" in_line layr-o1 main overlay create zulip --readers @layr-sap -q
ok "and moves the ignored key into it" in_line layr-o1 main overlay move -q zulip .zulip.env
ok "commit" in_line layr-o1 main commit -qm "zulip key"
eq "main shows both overlays" "keychain zulip" "$(in_line layr-o1 main overlay use | cut -f1 | paste -sd' ')"
ok "a group member makes a line from main" in_line layr-o2 main branch -q g2 main
eq "the member's line has the key" "ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY" "$(sh_in layr-o2 g2 cat .zulip.env)"
no "but not the owner's private loader" test -e $P/g2/.kc-renamed
eq "the member's line is clean" "" "$(in_line layr-o2 g2 status --porcelain)"
ok "the member reads the overlay's history" in_line layr-o2 g2 overlay log zulip
ok "a non-member makes a line from main" in_line layr-o3 main branch -q g3 main
no "the non-member's line lacks the key" test -e $P/g3/.zulip.env
no "the non-member cannot read the overlay" in_line layr-o3 g3 overlay log zulip
ok "the .envrc of the shared set is there" test -e $P/g3/.envrc
# The member changes the key in its line: its own overlay line, the shared set unchanged.
sh_in layr-o2 g2 "echo 'ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY-2' > .zulip.env && $L commit -qam 'new key'"
eq "the member's overlay has the new key" "ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY-2" "$(in_line layr-o2 g2 overlay show zulip HEAD:.zulip.env)"
eq "the owner's line keeps its key" "ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY" "$(sh_in layr-o1 main cat .zulip.env)"
eq "the shared set of g2 has no key" "0" "$(in_line layr-o2 g2 log -p --format= | grep -c ZULIP_APEIRO)"
# The owner sets the readers.
no "a reader cannot set the readers" in_line layr-o2 g2 overlay readers zulip @layr-sap layr-o2 layr-o3
ok "the owner can" in_line layr-o1 main overlay readers zulip @layr-sap layr-o3
ok "the new reader reads the history" in_line layr-o3 g3 overlay log zulip
ok "and can use the overlay" in_line layr-o3 g3 overlay use zulip
eq "the new reader's line gets the owner's key" "ZULIP_APEIRO_SHOWROOM_API_KEY=$KEY" "$(sh_in layr-o3 g3 cat .zulip.env)"
ok "a line can stop showing it" in_line layr-o3 g3 overlay use --none
no "the file leaves the folder" test -e $P/g3/.zulip.env
sh_in layr-o3 g3 "echo MY_OWN_KEY=1 > .zulip.env"
has "use does not overwrite a local ignored file" "would be overwritten" "$(in_line layr-o3 g3 overlay use --add zulip 2>&1)"
eq "the user's file stays" "MY_OWN_KEY=1" "$(sh_in layr-o3 g3 cat .zulip.env)"
sh_in layr-o3 g3 "rm .zulip.env"

# 8. Follow mode: the overlay line branches with the line; a merge of lines merges it.
in_line layr-o1 main branch -q f1 main
in_line layr-o1 f1 overlay use -q --none
sh_in layr-o1 f1 "mkdir -p private && echo 'note 1' > private/notes.md"
ok "an overlay in follow mode" in_line layr-o1 f1 overlay create notes --mode follow -q
in_line layr-o1 f1 overlay move -q notes private
ok "commit" in_line layr-o1 f1 commit -qm "private notes"
ok "a line from f1" in_line layr-o1 f1 branch -q f2 f1
eq "it has the notes" "note 1" "$(sh_in layr-o1 f2 cat private/notes.md)"
sh_in layr-o1 f2 "echo 'note 2' >> private/notes.md && echo shared > s2.txt && $L add -A && $L commit -qm 'notes and shared'"
eq "f1 keeps its notes" "note 1" "$(sh_in layr-o1 f1 cat private/notes.md)"
out=$(in_line layr-o1 f1 merge f2 2>&1); rc=$?
eq "f1 merges f2" "0" "$rc"
eq "the notes came with the merge" "note 2" "$(sh_in layr-o1 f1 tail -1 private/notes.md)"
ok "and the shared file" test -e $P/f1/s2.txt
eq "f1 is clean" "" "$(in_line layr-o1 f1 status --porcelain)"
no "the shared set has no notes" in_line layr-o1 f1 show HEAD:private/notes.md
# Both sides changed the notes: a three-way merge of the overlay.
sh_in layr-o1 f1 "printf 'top\n' | cat - private/notes.md > n && mv n private/notes.md && $L commit -qam 'top line'"
sh_in layr-o1 f2 "echo 'note 3' >> private/notes.md && $L commit -qam 'note 3'"
ok "a merge of diverged overlay lines" in_line layr-o1 f1 merge -q f2
eq "both changes are in" "top|note 1|note 2|note 3" "$(sh_in layr-o1 f1 cat private/notes.md | paste -sd'|')"
eq "and in the overlay's history" "top|note 1|note 2|note 3" "$(in_line layr-o1 f1 overlay show notes HEAD:private/notes.md | paste -sd'|')"
eq "f1 is clean after the merge" "" "$(in_line layr-o1 f1 status --porcelain)"
# A conflict in a follow overlay: listed by status, resolved by a commit.
sh_in layr-o1 f1 "sed -i 's/^note 1\$/note 1 (f1)/' private/notes.md && $L commit -qam 'f1 edit'"
sh_in layr-o1 f2 "sed -i 's/^note 1\$/note 1 (f2)/' private/notes.md && $L commit -qam 'f2 edit'"
eq "a conflicting overlay merge ends with 1" "1" "$(in_line layr-o1 f1 merge -q f2 >/dev/null 2>&1; echo $?)"
eq "status lists the conflict" "UU private/notes.md" "$(in_line layr-o1 f1 status --porcelain)"
sh_in layr-o1 f1 "printf 'top\nnote 1 (both)\nnote 2\nnote 3\n' > private/notes.md && $L commit -qam resolved"
eq "the resolution clears it" "" "$(in_line layr-o1 f1 status --porcelain)"
eq "the overlay has the resolution" "note 1 (both)" "$(in_line layr-o1 f1 overlay show notes HEAD:private/notes.md | sed -n 2p)"
# A merge of a line with a follow overlay: the target shows it, and the merger reads it.
in_line layr-o1 main branch -q m1 main
has "a merge needs the overlay in the target" "does not use" "$(in_line layr-o1 m1 merge f2 2>&1)"
in_line layr-o2 main branch -q m2 main
has "and a merger who reads it" "needs the read right of its overlay 'notes'" "$(in_line layr-o2 m2 merge f2 2>&1)"

# 9. Undo of an overlay commit moves the overlay line back.
sh_in layr-o1 f1 "echo 'note 4' >> private/notes.md && $L commit -qam 'note 4'"
ok "undo the commit" in_line layr-o1 f1 undo
eq "the change is in the folder again" " M private/notes.md" "$(in_line layr-o1 f1 status --porcelain)"
eq "the overlay is back" "note 3" "$(in_line layr-o1 f1 overlay show notes HEAD:private/notes.md | tail -1)"
# A refused undo leaves the overlay lines as they were.
sh_in layr-o1 f1 "$L commit -qam 'note 4 again' && echo s >> s2.txt && $L commit -qam 'shared after'"
id=$(in_line layr-o1 f1 op log -n 2 | awk 'NR==2{print $1}')
has "an undo of an older commit is refused" "changed after this operation" "$(in_line layr-o1 f1 undo $id 2>&1)"
eq "and the overlay keeps its state" "note 4" "$(in_line layr-o1 f1 overlay show notes HEAD:private/notes.md | tail -1)"
ok "the line still commits to the overlay" sh_in layr-o1 f1 "echo 'note 5' >> private/notes.md && $L commit -qam 'note 5'"
# A move waits for the next commit, whatever happens to the index.
sh_in layr-o1 f1 "echo draft > private/draft.md"
in_line layr-o1 f1 overlay move -q notes private/draft.md
ok "reset" in_line layr-o1 f1 reset -q
has "the move stays after a reset" "notes: private/draft.md" "$(in_line layr-o1 f1 status)"
in_line layr-o1 f1 save -q
no "the automatic state leaves the file out" in_line layr-o1 f1 show HEAD@{0}:private/draft.md
ok "commit" in_line layr-o1 f1 commit -qm draft
ok "the file is in the overlay" in_line layr-o1 f1 overlay show notes HEAD:private/draft.md
no "not in the shared set" in_line layr-o1 f1 show HEAD:private/draft.md
# A line with uncommitted changes of overlay files is deleted only with -f.
in_line layr-o1 f1 branch -q f3 f1
sh_in layr-o1 f3 "echo x >> private/notes.md"
has "a delete refuses" "uncommitted changes of overlay files" "$(in_line layr-o1 f1 branch -D f3 2>&1)"
ok "with -f it deletes" in_line layr-o1 f1 branch -D -f f3

# 10. A file of the shared set moves into an overlay: the next commit takes it out of the shared
# set (earlier states keep it). Automatic states that held it can go with --purge.
in_line layr-o1 o1t save -q --force
ok "the automatic state holds the shared file" in_line layr-o1 o1t show HEAD@{0}:notes.txt
out=$(in_line layr-o1 o1t overlay move --purge keychain notes.txt)
has "move notes the shared history" "earlier states keep it" "$out"
has "and deletes the automatic states that held it" "automatic state(s) of line o1t that held the files" "$out"
no "the automatic state is gone" in_line layr-o1 o1t show HEAD@{0}:notes.txt
ok "commit" in_line layr-o1 o1t commit -qm "notes are private now"
no "the shared set no longer has it" in_line layr-o1 o1t show HEAD:notes.txt
ok "the previous shared state still has it" in_line layr-o1 o1t show HEAD~1:notes.txt
ok "the overlay has it" in_line layr-o1 o1t overlay show keychain HEAD:notes.txt
ok "the folder keeps it" test -e $P/o1t/notes.txt
eq "the line is clean" "" "$(in_line layr-o1 o1t status --porcelain)"
# A delete of an overlay file goes to the overlay.
sh_in layr-o1 o1t "rm .kc-renamed && $L commit -qam 'drop the loader'"
no "the overlay lost the file" in_line layr-o1 o1t overlay show keychain HEAD:.kc-renamed
eq "the line is clean after the delete" "" "$(in_line layr-o1 o1t status --porcelain)"

# 11. The apply-method commit on a tree of 10,000 files (rough numbers, printed).
mkdir -p $T/big
for d in $(seq 1 100); do mkdir -p $T/big/d$d; for f in $(seq 1 100); do echo "$d $f" > $T/big/d$d/f$f.txt; done; done
l init big --from $T/big -q >/dev/null
l --project big access grant layr-o1 writer
as layr-o1 $L --project big branch -q plain main
as layr-o1 $L --project big branch -q stacked main
sh_in_big() { local line=$1; shift; as layr-o1 sh -c "cd $LAYR_ROOT/projects/big/lines/$line && $*"; }
sh_in_big stacked "$L overlay create -q priv && $L overlay move -q priv d1 d2 d3 d4 d5 d6 d7 d8 d9 d10 && $L commit -qm 'priv'"
eq "1,000 files in the overlay" "1000" "$(as layr-o1 $L --project big --line stacked overlay ls-files priv | wc -l)"
ms() { local a b; a=$(date +%s%N); "$@" >/dev/null; b=$(date +%s%N); echo $(( (b - a) / 1000000 )); }
for line in plain stacked; do sh_in_big $line "for i in 1 2 3 4 5; do echo x >> d1/f\$i.txt; echo y >> d50/f\$i.txt; done"; done
tp=$(ms as layr-o1 $L --project big --line plain commit -qam ten)
tsx=$(ms as layr-o1 $L --project big --line stacked commit -qam ten)
echo "INFO commit of 10 changed files in a tree of 10,000: ${tp} ms without overlays, ${tsx} ms with an overlay of 1,000 files (both layers changed)"
eq "the stacked commit changed both layers" "1" "$(as layr-o1 $L --project big --line stacked log --oneline | grep -c ten)"
ok "the commit stays under 5 s" test "$tsx" -lt 5000
tst=$(ms as layr-o1 $L --project big --line stacked status --porcelain)
echo "INFO status of the clean stacked line: ${tst} ms"
eq "fsck of the large project" "0 problem(s)" "$(l --project big fsck | tail -1)"

ok "gc" l --project sap gc -q
eq "main still shows its overlays after gc" "" "$(in_line layr-o1 main status --porcelain)"
eq "fsck is clean at the end" "0 problem(s)" "$(l --project sap fsck | tail -1)"
ok "reindex" l --project sap reindex
eq "fsck after reindex" "0 problem(s)" "$(l --project sap fsck | tail -1)"
eq "the stacks survive the reindex" "keychain zulip" "$(in_line layr-o1 main overlay use | cut -f1 | paste -sd' ')"
finish
