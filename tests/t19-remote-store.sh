#!/bin/bash
# Encrypted stores preserve signed history and recover file trees without a peer.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/source"
l init project --from "$T/source" -q >/dev/null
cd "$LAYR_ROOT/projects/project/lines/main"
url=${REMOTE_URL:-file://$T/archive}
l remote add archive "$url" --paper-key-out "$T/paper" >/dev/null
ok "first store push" l push archive --json
ok "remote signatures and file hashes" l fsck --remote archive --files
first=$(l rev-parse HEAD)
count_before=$(find "$T/archive/states" -name '*.tree.age' 2>/dev/null | wc -l)
ok "repeat push" l push archive -q
if [ -d "$T/archive/states" ]; then eq "repeat push publishes no second attempt" "$count_before" "$(find "$T/archive/states" -name '*.tree.age' | wc -l)"; fi
printf 'second state\n' >> notes.txt
ln -s notes.txt link.txt
printf '#!/bin/sh\nexit 0\n' > executable
chmod 755 executable
l add -A
l commit -m second -q
second=$(l rev-parse HEAD)
ok "incremental store push" l push archive -q
ok "all remote frames verify" l fsck --remote archive --files --json
has "remote history includes message" second "$(l log --remote archive)"
# Another local store has another machine key. The paper slot permits the initial clone.
original_root=$LAYR_ROOT
new_fs "$T/replica" 8G
export LAYR_ROOT=$T/replica
export LAYR_PAPER_KEY_FILE=$T/paper
cd "$T"
if l clone "$url" replica -q > "$T/clone.out" 2> "$T/clone.err"; then
    eq "clone with a paper key" 0 0
else
    cat "$T/clone.err"
    exit 1
fi
cd "$LAYR_ROOT/projects/replica/lines/main"
eq "clone selects the newest line head" "$second" "$(l rev-parse HEAD)"
has "restored file content" 'second state' "$(cat notes.txt)"
eq "restored symlink" notes.txt "$(readlink link.txt)"
ok "restored executable mode" test -x executable
ok "first state remains readable" l show "$first:notes.txt"
ok "local record verification after clone" l fsck
ok "clone has a machine slot" l fsck --remote origin --files
unset LAYR_PAPER_KEY_FILE
ok "repeat fetch with the machine slot" l fetch origin -q
ok "remote collection retains complete attempts" l gc --remote origin --json
ok "files remain valid after collection" l fsck --remote origin --files
# A new clone keeps both histories even if one source line is removed later.
export LAYR_ROOT=$original_root
export LAYR_PROJECT=project
export LAYR_LINE=main
cd "$original_root/projects/project/lines/main"
l branch temporary
cd "$original_root/projects/project/lines/temporary"
printf 'unfinished work\n' >> notes.txt
cd "$original_root/projects/project/lines/main"
l branch -D temporary
ok "store includes the deletion undo state by default" l push archive -q
export LAYR_ROOT=$T/replica
export LAYR_PROJECT=replica
export LAYR_LINE=main
cd "$LAYR_ROOT/projects/replica/lines/main"
ok "fetch after a source line deletion" l fetch origin -q
ok "complete remote history still verifies" l fsck --remote origin --files
finish
