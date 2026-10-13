#!/bin/bash
# .layrattributes: the folders that it makes regenerable (gitignore patterns) become caches on
# save --caches, if the ignore rules ignore them; a new line takes the nearest saved copy while
# the attribute is still set; a folder whose files are in history never becomes a cache, also
# when the ignore rules ignore it, and one that an older layr made a cache becomes plain again.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
in_line() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=r LAYR_LINE=$line $T/layr "$@"; }
L=$T/layr
S=$T/src
rm -rf $S && mkdir -p $S/src $S/web
echo code > $S/src/main.js && echo web > $S/web/app.js
printf 'node_modules/\n.venv/\ndata/\n' > $S/.gitignore
printf '# build output\nnode_modules regenerable\n.venv regenerable\nsrc regenerable\n' > $S/.layrattributes
l init r --from $S --owner layr-admin -q >/dev/null
P=$LAYR_ROOT/projects/r/lines
l --project r access grant layr-u1 writer
l --project r protect main --direct
cd /tmp || exit 1
tip() { in_line $1 $2 rev-parse HEAD; }

# 1. A build in main makes plain folders; save --caches turns the named ones into caches. The
#    ignored data/ folder that is not named changes too, but it is not a change of main.
as layr-admin sh -c "cd $P/main && mkdir -p node_modules/pkg web/node_modules/x .venv/bin data && echo 1 > node_modules/pkg/index.js && echo 2 > web/node_modules/x/i.js && echo py > .venv/bin/python && echo d > data/d"
mkdir -p $T/outside/node_modules && as layr-admin ln -s $T/outside $P/main/data/linked
head_before=$(tip layr-admin main)
ok "save --caches on main" in_line layr-admin main save -q --caches
eq "the head of main did not move" "$head_before" "$(tip layr-admin main)"
eq "the named folders are caches, none behind a link" "$(printf '.venv\tcache\nnode_modules\tcache\nweb/node_modules\tcache')" "$(in_line layr-admin main cache list)"
ok "a nested named folder is a subvolume" sh -c "btrfs subvolume show $P/main/web/node_modules >/dev/null"
no "an ignored folder that is not named stays a folder" sh -c "btrfs subvolume show $P/main/data >/dev/null 2>&1"
no "a named folder with files in history is no cache" sh -c "btrfs subvolume show $P/main/src >/dev/null 2>&1"
eq "its files stay in the state" "code" "$(in_line layr-admin main show HEAD:src/main.js)"

# 2. A new line of another user starts warm from the protected main, and owns its copy.
as layr-u1 $L --project r branch -q a1 main
eq "a new line takes the saved folder" "1" "$(cat $P/a1/node_modules/pkg/index.js)"
eq "and the nested one" "2" "$(cat $P/a1/web/node_modules/x/i.js)"
eq "and a folder that starts with a dot" "py" "$(cat $P/a1/.venv/bin/python)"
eq "the copy belongs to the line owner" "layr-u1" "$(stat -c %U $P/a1/web/node_modules/x/i.js)"

# 3. After main moves on, a new line still takes the copy of the nearest ancestor.
as layr-admin sh -c "cd $P/main && echo two >> src/main.js && $L commit -qam two"
as layr-u1 $L --project r branch -q a2 main
eq "a line from a later head starts warm" "1" "$(cat $P/a2/node_modules/pkg/index.js)"

# 4. When .layrattributes unsets the attribute of a folder, a new line does not take it.
as layr-admin sh -c "cd $P/main && printf '*/ regenerable\n.venv -regenerable\n' > .layrattributes && $L commit -qam narrower"
as layr-u1 $L --project r branch -q a3 main
eq "the still named folder comes" "1" "$(cat $P/a3/node_modules/pkg/index.js)"
no "a folder that is no longer named does not" test -e $P/a3/.venv/bin/python

# 5. An ignored, regenerable folder whose files are in the head state (added with add -f, or
#    kept by an import with --include-ignored) holds versioned files: it is no cache.
#    save --caches says so and leaves it a plain folder, so its files stay in history.
V=$T/v
rm -rf $V && mkdir -p $V/artifacts
echo code > $V/a.txt && echo kept > $V/artifacts/kept.txt
printf 'artifacts/\nout/\n' > $V/.gitignore
printf 'artifacts regenerable\nout regenerable\n' > $V/.layrattributes
l init v --from $V --owner layr-admin --include-ignored -q >/dev/null
l --project v access grant layr-u1 writer
l --project v protect main --direct
PV=$LAYR_ROOT/projects/v/lines
in_v() { local u=$1 line=$2; shift 2; as $u env LAYR_PROJECT=v LAYR_LINE=$line $T/layr "$@"; }
is_cache() { btrfs subvolume show "$1" >/dev/null 2>&1; }
NOT="not a cache: has versioned files; remove them from history first"
AGAIN="not a cache: has versioned files; a plain folder again"
as layr-admin sh -c "cd $PV/main && mkdir out && echo bin > out/bin && echo new > artifacts/new.bin"
eq "save --caches says why a folder is no cache" "artifacts: $NOT" "$(in_v layr-admin main save --caches | grep '^artifacts')"
ok "the other folder is a cache" is_cache $PV/main/out
no "the folder with versioned files stays a plain folder" is_cache $PV/main/artifacts
eq "cache list says why" "$(printf 'artifacts\t%s\nout\tcache' "$NOT")" "$(in_v layr-admin main cache list)"
eq "its files stay in the working content" "" "$(in_v layr-admin main status -s)"
as layr-u1 $L --project v branch -q v1 main
eq "a new line takes the folder from the state" "kept" "$(cat $PV/v1/artifacts/kept.txt)"
no "without the build output of main" test -e $PV/v1/artifacts/new.bin
no "as a plain folder" is_cache $PV/v1/artifacts
eq "and the other cache warm" "bin" "$(cat $PV/v1/out/bin)"

# 6. cache repair and cache add leave such a folder plain. One that an older layr made a cache
#    (a subvolume with the same files) becomes a plain folder again.
eq "cache repair leaves it plain" "" "$(in_v layr-admin main cache repair)"
no "and a plain folder" is_cache $PV/main/artifacts
eq "cache add says why" "artifacts: $NOT" "$(in_v layr-admin main cache add artifacts)"
no "and leaves it a plain folder" is_cache $PV/main/artifacts
l --project v cache remove artifacts
ok "an automatic state" in_v layr-admin main save -q --force -m quiet
eq "holds its files" "kept" "$(in_v layr-admin main show 'HEAD@{0}:artifacts/kept.txt')"
older_cache() { # older_cache <line folder> <path>: as an older layr did
    btrfs -q subvolume create "$1/$2.old" && cp -a --reflink=always "$1/$2/." "$1/$2.old/" &&
        rm -rf "${1:?}/$2" && mv "$1/$2.old" "$1/$2" && chown layr-admin: "$1/$2"
}
older_cache $PV/main artifacts && echo stale > $PV/main/artifacts/stale.bin
eq "cache list asks for a repair" "$(printf 'artifacts\tnot a cache: has versioned files (run layr cache repair)\nout\tcache')" "$(in_v layr-admin main cache list)"
eq "cache repair makes it a plain folder again" "artifacts: $AGAIN" "$(in_v layr-admin main cache repair)"
no "a plain folder" is_cache $PV/main/artifacts
eq "with all its files" "kept stale" "$(cat $PV/main/artifacts/kept.txt $PV/main/artifacts/stale.bin | xargs)"
eq "none of them deleted" "" "$(in_v layr-admin main status -s)"
older_cache $PV/main artifacts
eq "save --caches does the same" "artifacts: $AGAIN" "$(in_v layr-admin main save --caches | grep '^artifacts')"
no "a plain folder after the save" is_cache $PV/main/artifacts
eq "its files in the working content" "" "$(in_v layr-admin main status -s)"

# 7. A folder that an earlier state saved as a cache, with versioned files since: a new line
#    takes the files from its state, not the older cache.
rm -rf $PV/main/out && mkdir $PV/main/out && echo keep > $PV/main/out/keep.txt && chown -R layr-admin: $PV/main/out
as layr-admin sh -c "cd $PV/main && $L add -f out/keep.txt && $L commit -qm 'keep out'"
eq "cache list says why" "$(printf 'artifacts\t%s\nout\t%s' "$NOT" "$NOT")" "$(in_v layr-admin main cache list)"
as layr-u1 $L --project v branch -q v2 main
eq "a new line takes the versioned file" "keep" "$(cat $PV/v2/out/keep.txt)"
no "not the older cache" test -e $PV/v2/out/bin
no "in a plain folder" is_cache $PV/v2/out
finish
