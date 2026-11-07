#!/bin/bash
# ci: macos overlayfs
# layr objects (docs/objects.md), the experimental content-addressed store: import a state as a
# tree and times table, import it again without new objects, verify each entry, byte and time
# against the state, check out a tree with its times, import the next state incrementally as a
# delta, repack into packs (also from layr gc), and find a corrupted object.
. "$(dirname "$0")/lib.sh"

sha_blob() { # sha_blob <file>: the object id of a file (SHA-256 of "blob <size>\0" and the bytes)
    python3 -I -c 'import hashlib,sys; d=open(sys.argv[1],"rb").read(); print(hashlib.sha256(b"blob %d\0" % len(d) + d).hexdigest())' "$1"
}
stat_mtime() { # stat_mtime <path>: the modification time in nanoseconds
    python3 -I -c 'import os,sys; print(os.lstat(sys.argv[1]).st_mtime_ns)' "$1"
}
json_field() { # json_field <field> <json>
    python3 -I -c 'import json,sys; print(json.loads(sys.argv[2])[sys.argv[1]])' "$1" "$2"
}

sample $T/src
mkdir -p $T/src/empty/deeper $T/src/bin
printf '#!/bin/sh\necho hi\n' > $T/src/bin/run.sh
chmod 755 $T/src/bin/run.sh
ln -s ../notes.txt $T/src/bin/notes-link
ln -s missing/target $T/src/dangling
python3 -I -c 'import random,sys; random.seed(7); sys.stdout.buffer.write(random.randbytes(3 << 20))' > $T/src/big.bin
ln $T/src/src/main.rs $T/src/main-hard.rs
# An extended attribute, where the file system and the tools allow one (verify compares them).
if [ "$(uname -s)" = Darwin ]; then
    xattr -w user.layr-test value $T/src/bin/run.sh
else
    python3 -I -c 'import os,sys; os.setxattr(sys.argv[1], "user.layr-test", b"value")' $T/src/bin/run.sh 2>/dev/null || true
fi
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
O=$LAYR_ROOT/projects/p/meta/objects
cd $P/main

# 1. Import: the tree id is printed first; the same state gives the same tree and no new object.
out=$(l objects import head 2>&1)
tree=$(echo "$out" | head -1)
eq "import prints a tree id" 64 "${#tree}"
again=$(l objects import head --json 2>&1)
eq "a second import gives the same tree" "$tree" "$(json_field tree "$again")"
eq "a second import adds no object" 0 "$(json_field new_objects "$again")"
eq "the blob id is SHA-256 of the typed content" yes \
    "$([ -f $O/blobs/$(sha_blob notes.txt | cut -c1-2)/$(sha_blob notes.txt | cut -c3-) ] && echo yes)"

# 2. Verify: a checkout of the tree equals the state entry for entry and byte for byte.
out=$(l objects verify head 2>&1)
has "verify passes on the imported state" "verified" "$out"
eq "verify leaves no copy in the work folder" "" "$(ls $LAYR_ROOT/projects/p/meta/work 2>/dev/null)"

# 3. Checkout by tree id and by revision.
ok "checkout of the tree" l objects checkout "$tree" $T/out
eq "file content" "$(cat notes.txt)" "$(cat $T/out/notes.txt)"
ok "large file content" cmp big.bin $T/out/big.bin
eq "symbolic link" ../notes.txt "$(readlink $T/out/bin/notes-link)"
eq "dangling symbolic link" missing/target "$(readlink $T/out/dangling)"
ok "the executable mode" test -x $T/out/bin/run.sh
ok "the empty folder" test -d $T/out/empty/deeper
no "ignored files stay out (not in the state)" test -e $T/out/debug.log
no "checkout refuses a folder with files" l objects checkout "$tree" $T/out
ok "checkout by revision" l objects checkout head $T/out2
eq "checkout restores the modification time" "$(stat_mtime notes.txt)" "$(stat_mtime $T/out/notes.txt)"
eq "and the time of a folder" "$(stat_mtime bin)" "$(stat_mtime $T/out/bin)"
no "checkout of an unknown tree" l objects checkout "$(printf 'ab%.0s' $(seq 32))" $T/out3

# 4. History: the next version of a file is a delta against the tree of the parent state.
first=$(l rev-parse head)
seq 1 400 | sed 's/^/more notes /' >> notes.txt
l save -a -m "more notes" -q
cold=$(l objects import head --cold --json 2>&1)
has "a cold import names the parent tree" "$tree" "$(json_field previous "$cold")"
full=$(l objects import head --cold --full --json 2>&1)
ok "an incremental import reads fewer files than a full one" test "$(json_field files "$cold")" -lt "$(json_field files "$full")"
eq "a full import gives the same tree" "$(json_field tree "$cold")" "$(json_field tree "$full")"
eq "and the same times" "$(json_field times "$cold")" "$(json_field times "$full")"
st=$(l objects stat --json)
eq "the changed file is a delta" 1 "$(json_field delta_blobs "$st")"
ok "verify of the cold state" l objects verify head --cold
has "stat names delta blobs" "delta blobs" "$(l objects stat)"

# 5. Repack: loose objects go into packs; every state still verifies; gc repacks too.
out=$(l objects repack --json 2>&1)
has "repack writes a pack" '"packs":1' "$out"
st=$(l objects stat --json)
eq "no loose object is left" 0 "$(json_field loose_bytes "$st")"
ok "verify of the packed state" l objects verify head --cold
ok "verify of the first state from packs" l objects verify "$first"
echo 'gc input' > gc.txt
l add gc.txt
l save -m "gc input" -q
l objects import head --cold >/dev/null
has "gc repacks the object store" "objects:" "$(l gc 2>&1)"
eq "gc left no loose object" 0 "$(json_field loose_bytes "$(l objects stat --json)")"

# 6. A corrupted object: the import keeps the stored object (no rewrite), so verify finds it.
blob=$O/blobs/$(sha_blob src/main.rs | cut -c1-2)/$(sha_blob src/main.rs | cut -c3-)
chmod u+w "$blob"
python3 -I -c 'import sys; p=sys.argv[1]; d=bytearray(open(p,"rb").read()); d[0]^=1; open(p,"wb").write(d)' "$blob"
out=$(l objects verify "$first" 2>&1); rc=$?
eq "verify of a corrupted store fails" 1 "$rc"
has "and names the file" "content differs: src/main.rs" "$out"

finish
