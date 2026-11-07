#!/bin/bash
# ci: macos overlayfs
# layr migrate (docs/objects.md, section 13): a project with history (lines, a merge, a rename,
# symbolic links, empty folders, an extended attribute, hard links, a park, a tag and unsaved work)
# moves to the object store. --dry-run changes nothing, a held lock refuses the run, an interrupted
# run continues with --resume, every state checks out equal to its snapshot (entries, bytes, modes,
# links and times, compared here by a second walk), and show, diff, line new and gc work after
# --delete-old removed the old snapshots.
. "$(dirname "$0")/lib.sh"

DB() { # DB <sql>: one query on the database of project p
    python3 -I -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); [print("|".join("" if v is None else str(v) for v in r)) for r in c.execute(sys.argv[2])]' \
        "$LAYR_ROOT/projects/p/meta/db/layr.sqlite" "$1"
}
uuid_of() { # uuid_of <state as layr prints it>: the id as the database and states/ name it
    DB "SELECT id FROM states WHERE display='$1'"
}
json_field() { # json_field <field> <json>
    python3 -I -c 'import json,sys; print(json.loads(sys.argv[2])[sys.argv[1]])' "$1" "$2"
}
# A second walk, independent of the Rust comparison: names, types, permission bits, bytes, link
# targets, modification times (nanoseconds, the root included) and hard link groups.
same_tree() { # same_tree <a> <b>: prints the first difference, exit 1 on one
    python3 -I - "$1" "$2" <<'EOF'
import os, stat, sys
a, b = sys.argv[1], sys.argv[2]
def walk(root):
    out, links = {}, {}
    for d, dirs, files in os.walk(root):
        for name in dirs + files:
            p = os.path.join(d, name)
            rel = os.path.relpath(p, root)
            if rel == ".git" or rel.startswith(".git/"):
                continue
            m = os.lstat(p)
            kind = "l" if stat.S_ISLNK(m.st_mode) else "d" if stat.S_ISDIR(m.st_mode) else "f"
            body = os.readlink(p) if kind == "l" else open(p, "rb").read() if kind == "f" else None
            mode = 0 if kind == "l" else m.st_mode & 0o7777
            out[rel] = (kind, mode, body, m.st_mtime_ns)
            if kind == "f" and m.st_nlink > 1:
                links.setdefault(m.st_ino, set()).add(rel)
    return out, sorted(sorted(g) for g in links.values() if len(g) > 1)
(ta, la), (tb, lb) = walk(a), walk(b)
if os.lstat(a).st_mtime_ns != os.lstat(b).st_mtime_ns:
    sys.exit(print("root time differs") or 1)
for rel in sorted(set(ta) | set(tb)):
    if ta.get(rel) != tb.get(rel):
        sys.exit(print("differs:", rel, ta.get(rel, "absent")[:2], tb.get(rel, "absent")[:2]) or 1)
if la != lb:
    sys.exit(print("hard links differ:", la, lb) or 1)
print(len(ta))
EOF
}
xattr_of() { # xattr_of <file>: the value of user.layr-test
    if [ "$(uname -s)" = Darwin ]; then xattr -p user.layr-test "$1" 2>/dev/null
    else python3 -I -c 'import os,sys; print(os.getxattr(sys.argv[1], "user.layr-test").decode())' "$1" 2>/dev/null; fi
}

# A project with history.
sample $T/src
mkdir -p $T/src/empty/deeper $T/src/bin
printf '#!/bin/sh\necho hi\n' > $T/src/bin/run.sh
chmod 755 $T/src/bin/run.sh
ln -s ../notes.txt $T/src/bin/notes-link
ln $T/src/notes.txt $T/src/notes-hard.txt
if [ "$(uname -s)" = Darwin ]; then
    xattr -w user.layr-test value $T/src/bin/run.sh
else
    python3 -I -c 'import os,sys; os.setxattr(sys.argv[1], "user.layr-test", b"value")' $T/src/bin/run.sh 2>/dev/null || true
fi
HAS_XATTR=$(xattr_of $T/src/bin/run.sh)
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main
first=$(l rev-parse head)
for i in 1 2 3; do
    seq 1 $((i * 50)) | sed "s/^/round $i /" >> notes.txt
    echo "file $i" > "file$i.txt"
    l add "file$i.txt"
    l save -a -m "round $i" -q
done
l mv file1.txt renamed.txt
ln -sfn file2.txt current
l add current
l save -a -m "rename and link" -q
l tag v1 head
tagged=$(l rev-parse v1)
l line new -q topic
cd $P/topic
echo topic > topic.txt
l add topic.txt
l save -m "topic 1" -q
chmod 755 file2.txt
l save -a -m "topic 2" -q
cd $P/main
echo "main side" >> file3.txt
l save -a -m "main side" -q
l merge topic
merged=$(l rev-parse head)
eq "the merge state has two parents" 2 "$(DB "SELECT COUNT(*) FROM parents WHERE state='$(uuid_of $merged)'")"
l line new -q side
cd $P/side
echo parked > parked.txt
l add parked.txt
l park -q
echo "unsaved work" > unsaved.txt
cd $P/main
heads_before=$(DB "SELECT name||' '||head FROM lines WHERE deleted=0 ORDER BY name")
refs_before=$(DB "SELECT name||' '||state FROM tags ORDER BY name")
sources=$(DB "SELECT COUNT(*) FROM states WHERE kind NOT IN ('cache','derived') AND deleted=0")

# 1. --dry-run changes nothing.
listing() { (cd $LAYR_ROOT/projects/p && find states deltas meta -path meta/db -prune -o -path meta/work -prune -o -path meta/scan -prune -o -print 2>/dev/null | grep -v 'lock' | sort); }
before=$(listing); ops=$(DB "SELECT COUNT(*) FROM records")
out=$(l migrate --dry-run 2>&1)
has "dry run says nothing changes" "dry run: nothing changes" "$out"
has "dry run names the states to migrate" "state(s) to migrate" "$out"
eq "dry run writes no record" "$ops" "$(DB "SELECT COUNT(*) FROM records")"
eq "dry run changes no file of the project" "$before" "$(listing)"
no "dry run makes no object store" test -e $LAYR_ROOT/projects/p/meta/objects
eq "dry run leaves the unsaved file unsaved" "?? unsaved.txt" "$(cd $P/side && l status --short 2>/dev/null | grep unsaved)"

# 2. A command that holds the project lock refuses the run.
(cd $P/main && LAYR_TEST_LOCK_HOLD_MS=4000 l tag held head) &
holder=$!
for _ in $(seq 1 50); do [ -s $LAYR_ROOT/projects/p/meta/lock.holder ] && break; sleep 0.1; done
out=$(l migrate 2>&1); rc=$?
eq "migrate refuses while a command holds the project lock" 1 "$rc"
has "and says why" "holds the project lock" "$out"
wait $holder
no "the refused run made no object store" test -e $LAYR_ROOT/projects/p/meta/objects

# 3. An interrupted run continues with --resume.
out=$(LAYR_TEST_MIGRATE_STOP_AFTER=3 l migrate 2>&1); rc=$?
eq "the injected interruption stops the run" 86 "$rc"
has "the run saved the unsaved work of a line folder first" "line side: unsaved work saved" "$out"
eq "the table holds the verified states" 4 "$(wc -l < $LAYR_ROOT/projects/p/meta/migrate/map | tr -d ' ')"
eq "no state record has a tree yet (the record batch did not run)" 0 "$(DB "SELECT COUNT(*) FROM states WHERE json_extract(rec,'\$.tree') IS NOT NULL")"
out=$(l migrate 2>&1); rc=$?
eq "a plain run refuses an interrupted migration" 1 "$rc"
has "and names --resume" "--resume" "$out"
out=$(l migrate --resume 2>&1); rc=$?
eq "--resume finishes" 0 "$rc"
has "it records the 3 states of the first run" "3 recorded from an earlier run" "$out"
sources=$(DB "SELECT COUNT(*) FROM states WHERE kind NOT IN ('cache','derived') AND deleted=0")
eq "every source state has a tree and a times table" "$sources" \
    "$(DB "SELECT COUNT(*) FROM states WHERE kind NOT IN ('cache','derived') AND deleted=0 AND length(json_extract(rec,'\$.tree'))=64 AND length(json_extract(rec,'\$.times'))=64")"
eq "the table has one line per state" "$((sources + 1))" "$(wc -l < $LAYR_ROOT/projects/p/meta/migrate/map | tr -d ' ')"
no "the run marker is gone" test -e $LAYR_ROOT/projects/p/meta/migrate/running
eq "line heads keep their state ids" "$heads_before" "$(DB "SELECT name||' '||head FROM lines WHERE deleted=0 ORDER BY name")"
eq "tags keep their state ids" "$refs_before" "$(DB "SELECT name||' '||state FROM tags WHERE name<>'held' ORDER BY name")"
eq "the repack left no loose object" 0 "$(json_field loose_bytes "$(l objects stat --json)")"
out=$(l migrate 2>&1)
has "a second run has nothing to do" "0 migrated now" "$out"

# 4. Every state, checked out again, equals its snapshot (a second walk, in Python).
checked=0; bad=""
for state in $(DB "SELECT id FROM states WHERE kind NOT IN ('cache','derived') AND deleted=0"); do
    recorded=$(DB "SELECT json_extract(rec,'\$.tree') FROM states WHERE id='$state'")
    v=$(l objects verify "$state" --cold --json 2>/dev/null)
    [ "$(json_field tree "$v" 2>/dev/null)" = "$recorded" ] || bad="$bad $state(tree)"
    rm -rf $T/co
    l objects checkout "$state" $T/co >/dev/null 2>&1 || bad="$bad $state(checkout)"
    snap=$LAYR_ROOT/projects/p/states/$state
    if [ -d "$snap" ]; then
        same_tree "$snap" $T/co >/dev/null || bad="$bad $state($(same_tree "$snap" $T/co))"
        checked=$((checked + 1))
    fi
done
eq "every checkout equals its snapshot, time for time" "" "$bad"
ok "at least one state per save was compared by the second walk" test "$checked" -ge 8
rm -rf $T/co
l objects checkout "$first" $T/co >/dev/null
eq "a hard link group stays a group" "$(stat_field inode $T/co/notes.txt)" "$(stat_field inode $T/co/notes-hard.txt)"
chmod -R u+w $T/co; rm -rf $T/co
l objects checkout "$tagged" $T/co >/dev/null
eq "a symbolic link stays a link" "file2.txt" "$(readlink $T/co/current)"
ok "an empty folder stays" test -d $T/co/empty/deeper
eq "an extended attribute stays" "$HAS_XATTR" "$(xattr_of $T/co/bin/run.sh)"
chmod -R u+w $T/co; rm -rf $T/co

# 5. Reads after --delete-old: the old snapshots go, the states come back from the object store.
out=$(l migrate --delete-old 2>&1)
has "--delete-old deletes the old snapshots" "old snapshots: deleted" "$out"
ok "the first state had a snapshot" test -n "$(uuid_of $first)"
no "the snapshot of the first state is gone" test -e $LAYR_ROOT/projects/p/states/$(uuid_of $first)
no "and it has no thin layer" test -e $LAYR_ROOT/projects/p/deltas/$(uuid_of $first)
eq "show reads a file of a deleted snapshot from the objects" "$(seq 1 20 | sed 's/^/line /')" "$(l show $first:notes.txt)"
no "and builds no snapshot for it" test -e $LAYR_ROOT/projects/p/states/$(uuid_of $first)
has "show lists a folder from the objects" "deeper/" "$(l show $first:empty)"
eq "show reads a link target" "file2.txt" "$(l show $tagged:current)"
no "show of a missing path fails" l show $first:missing.txt
eq "show of a line head still works" "topic" "$(l show topic:topic.txt)"
# On APFS a thin layer that a kept layer builds on stays; the first state has no snapshot now.
l line new -q old "$first"
eq "line new from a deleted snapshot checks out the tree" "$(seq 1 20 | sed 's/^/line /')" "$(cat $P/old/notes.txt)"
no "and builds no snapshot for the state" test -e $LAYR_ROOT/projects/p/states/$(uuid_of $first)
no "nor a thin layer" test -e $LAYR_ROOT/projects/p/deltas/$(uuid_of $first)
eq "with the times of the state" "$(python3 -I -c 'import os,sys; print(os.lstat(sys.argv[1]).st_mtime_ns)' $P/main/bin/run.sh)" \
    "$(python3 -I -c 'import os,sys; print(os.lstat(sys.argv[1]).st_mtime_ns)' $P/old/bin/run.sh)"
eq "the new line is clean" "" "$(cd $P/old && l status --short)"
has "diff between two migrated states (snapshots made again from the objects)" "+file 2" "$(l diff $first $tagged)"
ok "fsck passes after the migration" l fsck
ok "gc passes after the migration" l gc

# New saves keep the portable state complete without another migration.
echo 'after migration' >> notes.txt
l save -a -m 'after migration' >/dev/null
new=$(l rev-parse head)
new_id=$(uuid_of "$new")
eq "new saves carry tree and times" "1" "$(DB "SELECT count(*) FROM states WHERE id='$new_id' AND json_extract(rec,'$.tree') IS NOT NULL AND json_extract(rec,'$.times') IS NOT NULL")"
ok "new saved objects verify" l objects verify "$new"
l objects checkout "$new" $T/new-checkout >/dev/null
same_tree "$P/main" "$T/new-checkout" >/dev/null
ok "new object content includes the edit" grep -q 'after migration' "$T/new-checkout/notes.txt"
ok "records with object fields survive replay" l fsck

finish
