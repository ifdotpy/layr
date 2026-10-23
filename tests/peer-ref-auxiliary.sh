#!/bin/bash
# Standalone cache and derived states have no CLI creation command.
# Set CARGO to its absolute path when sudo removes Cargo from PATH.
. "$(dirname "$0")/lib.sh"
new_fs "$T/peer" 4G
if LAYR_TEST_AUX_ROOT="$LAYR_ROOT" LAYR_TEST_AUX_PEER_ROOT="$T/peer/store" LAYR_TEST_BINARY="$LAYR" \
    "${CARGO:-cargo}" test --locked auxiliary_states_cross_the_peer_protocol -- --ignored > "$T/auxiliary.log" 2>&1; then
    PASS=$((PASS + 1))
    echo "PASS cache and derived states cross the peer protocol"
else
    cat "$T/auxiliary.log"
    FAIL=$((FAIL + 1))
    echo "FAIL cache and derived states cross the peer protocol"
fi
finish
