#!/bin/bash
# ci: macos overlayfs
# layr objects (docs/objects.md), the experimental content-addressed store: import a state as a
# tree, import it again without new objects, verify each entry and byte against the state, check
# out a tree with clones, keep the next version as a delta, and find a corrupted object.
. "$(dirname "$0")/lib.sh"

sha_blob() { # sha_blob <file>: the object id of a file (SHA-256 of "blob <size>\0" and the bytes)
    python3 -I -c 'import hashlib,sys; d=open(sys.argv[1],"rb").read(); print(hashlib.sha256(b"blob %d\0" % len(d) + d).hexdigest())' "$1"
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
no "checkout of an unknown tree" l objects checkout "$(printf 'ab%.0s' $(seq 32))" $T/out3

# 4. History: the next version of a file is a delta against the tree of the parent state.
first=$(l rev-parse head)
seq 1 400 | sed 's/^/more notes /' >> notes.txt
l save -a -m "more notes" -q
cold=$(l objects import head --cold --json 2>&1)
has "a cold import names the parent tree" "$tree" "$(json_field previous "$cold")"
st=$(l objects stat --json)
eq "the changed file is a delta" 1 "$(json_field delta_blobs "$st")"
ok "verify of the cold state" l objects verify head --cold
has "stat names delta blobs" "delta blobs" "$(l objects stat)"

# 5. A corrupted object: the import keeps the stored object (no rewrite), so verify finds it.
blob=$O/blobs/$(sha_blob src/main.rs | cut -c1-2)/$(sha_blob src/main.rs | cut -c3-)
chmod u+w "$blob"
python3 -I -c 'import sys; p=sys.argv[1]; d=bytearray(open(p,"rb").read()); d[0]^=1; open(p,"wb").write(d)' "$blob"
out=$(l objects verify "$first" 2>&1); rc=$?
eq "verify of a corrupted store fails" 1 "$rc"
has "and names the file" "content differs: src/main.rs" "$out"

finish
