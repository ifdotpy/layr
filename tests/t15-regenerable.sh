#!/bin/bash
# .regenerable: the folders it names (gitignore patterns) become layers on save --layers, if the
# ignore rules ignore them; a new line takes the nearest saved copy while the file still names
# the folder; a folder whose files are in history never becomes a layer.
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
printf 'node_modules\n.venv\nsrc\n' > $S/.regenerable
l init r --from $S --owner layr-admin -q >/dev/null
P=$LAYR_ROOT/projects/r/lines
l --project r access grant layr-u1 writer
l --project r protect main --direct
cd /tmp || exit 1
tip() { in_line $1 $2 rev-parse HEAD; }

# 1. A build in main makes plain folders; save --layers turns the named ones into layers. The
#    ignored data/ folder that is not named changes too, but it is not a change of main.
as layr-admin sh -c "cd $P/main && mkdir -p node_modules/pkg web/node_modules/x .venv/bin data && echo 1 > node_modules/pkg/index.js && echo 2 > web/node_modules/x/i.js && echo py > .venv/bin/python && echo d > data/d"
mkdir -p $T/outside/node_modules && as layr-admin ln -s $T/outside $P/main/data/linked
head_before=$(tip layr-admin main)
ok "save --layers on main" in_line layr-admin main save -q --layers
eq "the head of main did not move" "$head_before" "$(tip layr-admin main)"
eq "the named folders are layers, none behind a link" "$(printf '.venv\tlayer\nnode_modules\tlayer\nweb/node_modules\tlayer')" "$(in_line layr-admin main layer list)"
ok "a nested named folder is a subvolume" sh -c "btrfs subvolume show $P/main/web/node_modules >/dev/null"
no "an ignored folder that is not named stays a folder" sh -c "btrfs subvolume show $P/main/data >/dev/null 2>&1"
no "a named folder with files in history is no layer" sh -c "btrfs subvolume show $P/main/src >/dev/null 2>&1"
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

# 4. When .regenerable no longer names a folder, a new line does not take it.
as layr-admin sh -c "cd $P/main && printf 'node_modules\n' > .regenerable && $L commit -qam narrower"
as layr-u1 $L --project r branch -q a3 main
eq "the still named folder comes" "1" "$(cat $P/a3/node_modules/pkg/index.js)"
no "a folder that is no longer named does not" test -e $P/a3/.venv/bin/python
finish
