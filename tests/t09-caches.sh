#!/bin/bash
# Regenerable caches: folders kept as nested subvolumes, outside automatic states and remote copies;
# carried for a live move with --caches.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init g --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cstate() { l overlay list | grep $TG | cut -f6; }
cd $P/main
l config regenerable '["target"]'
l line new -q w
cd $P/w
ok "a new line gets the cache as a subvolume" sh -c "btrfs subvolume show $P/w/target >/dev/null"
head -c 20000000 /dev/urandom > target/big.bin; echo src >> notes.txt
l save -q
eq "automatic states do not hold the cache content" "" "$(l show head@{0}:target | tail -n +3)"
eq "status ignores the cache" " M notes.txt" "$(l status -s)"
l save -q --glasses
eq "save --glasses makes the cache overlay" "cache" "$(cstate)"
rm -rf target; mkdir target; echo rebuilt > target/out
has "a tool replaced the cache with a plain folder" "plain folder" "$(cstate)"
l overlay repair $TG >/dev/null; l save -q
eq "overlay repair makes it a cache again" "cache" "$(cstate)"
eq "and keeps its content" "rebuilt" "$(cat target/out)"
# A live move with warm build output. The first sync sends the state without caches; the next
# sync with must still send them. Caches go only to machines the project trusts: the
# target gets trusted by the syncs, after the cache save, so the sync adds it to the readers. The
# regenerable setting belongs to this machine: a .layrattributes file makes target a cache there too.
new_fs $T/b 4G
# This option is for a disposable Linux VM. The test stays unprivileged; only
# the peer server, its daemon, and their fixture cleanup run as root.
peer_service=${LAYR_TEST_PEER_ROOT_SERVICE:-0}
peer_cmd=(env "LAYR_ROOT=$T/b/store" "$LAYR" serve-peer g)
if [ "$peer_service" = 1 ]; then
    [ "$(uname -s)" = Linux ] && [ "$(id -u)" != 0 ] || { echo 'FATAL: peer service fixture requires a non-root Linux test' >&2; exit 1; }
    [ "${LAYR_TEST_BTRFS_CLEANUP:-}" = 1 ] || { echo 'FATAL: peer service fixture requires isolated Btrfs cleanup' >&2; exit 1; }
    test_base=$(cd "${TMPDIR:?}" && pwd -P) || exit 1
    case "$T" in "$test_base"/layr-test.*) test_name=${T#"$test_base"/} ;; *) echo 'FATAL: peer fixture is outside TMPDIR' >&2; exit 1 ;; esac
    case "$test_name" in */*) echo 'FATAL: invalid peer fixture path' >&2; exit 1 ;; esac
    peer_cmd=(sudo -n env "LAYR_ROOT=$T/b/store" "$LAYR" serve-peer g)
    peer_cleanup() {
        local result=$? daemon_pid
        trap - EXIT
        if [ -n "${PEER_SUPERVISOR:-}" ]; then
            if [ -s "$T/peer.pid" ]; then
                daemon_pid=$(cat "$T/peer.pid")
                case "$daemon_pid" in *[!0-9]*|'') echo 'FATAL: invalid peer daemon pid' >&2; result=1 ;; *) sudo -n kill -TERM "$daemon_pid" 2>/dev/null || true ;; esac
            fi
            wait "$PEER_SUPERVISOR" 2>/dev/null || result=1
        fi
        if [ -d "$T/b" ]; then
            mkdir "$T/peer-trash" && mv "$T/b" "$T/peer-trash/b" && sudo -n "$LAYR" __empty-trash "$T/peer-trash" || result=1
            if [ -n "$(ls -A "$T/peer-trash" 2>/dev/null)" ] || [ -s "$T/trash.errors" ]; then
                echo "FATAL: peer fixture remains in $T/peer-trash" >&2
                result=1
            fi
        fi
        cleanup
        exit "$result"
    }
    trap peer_cleanup EXIT
fi
echo 'target regenerable' > .layrattributes; l add .layrattributes; l save -q -m "target is a cache"
head -c 1000000 /dev/urandom > target/warm.bin
# The regenerable setting belongs to this machine: .layrattributes makes target a cache on the peer too.
echo 'target regenerable' > .layrattributes
l add .layrattributes; l save -q -m "target is a cache"
l save -q --glasses
# Caches go only to machines the project trusts (docs/cache-overlays.md).
peer_self=$(env LAYR_ROOT=$T/b/store $LAYR remote self --json)
l remote add test-peer ssh://test/g >/dev/null
ok "trust the peer" l remote trust test-peer "$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')" \
    --public-key "$(echo "$peer_self" | python3 -c 'import json,sys; print(json.load(sys.stdin)["public_key"])')"
b_id=$(env LAYR_ROOT=$T/b/store $LAYR remote self --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
peer_exchange sync g -q --auto -- "${peer_cmd[@]}"
peer_exchange sync g -q --auto -- "${peer_cmd[@]}"
has "the sync adds the trusted target to the cache readers" "machine:$b_id" "$(l overlay readers $TG)"
if [ "$peer_service" = 1 ]; then
    sudo -n sh -c 'echo $$ > "$1"; shift; exec "$@"' sh "$T/peer.pid" env LAYR_ROOT="$T/b/store" "$LAYR" daemon --socket "$T/peer.sock" --tick 3600 > "$T/peer-daemon.log" 2>&1 &
    PEER_SUPERVISOR=$!
    for _ in $(seq 1 100); do [ -S "$T/peer.sock" ] && break; sleep 0.1; done
    [ -S "$T/peer.sock" ] || { cat "$T/peer-daemon.log"; echo 'FATAL: peer daemon did not start' >&2; exit 1; }
    env -u LAYR_DIRECT LAYR_ROOT="$T/b/store" LAYR_SOCKET="$T/peer.sock" "$LAYR" --project g adopt w >/dev/null
else
    env LAYR_ROOT=$T/b/store $LAYR --project g adopt w >/dev/null
fi
eq "the moved line has the build output" "$(md5sum < target/warm.bin)" "$(md5sum < $T/b/store/projects/g/lines/w/target/warm.bin)"
ok "and it is a cache there" sh -c "btrfs subvolume show $T/b/store/projects/g/lines/w/target >/dev/null"
finish
