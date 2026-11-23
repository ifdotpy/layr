#!/bin/bash
# `layr wait` (docs/remotes.md, "Wait for a change"): the long-poll of the project events needs a Layr
# Cloud remote. The live answers (start, change, timeout, resync) are covered by the unit tests of
# src/cmd/waitcmd.rs and the gateway client.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
mkdir -p "$T/store"
l remote add files "file://$T/store" >/dev/null 2>&1
has "a file remote has no events" "needs a Layr Cloud remote" "$(l wait files --timeout 1 2>&1)"
l config cloud.remote.archived.url s3://wait-fixture >/dev/null
has "a finite wait rejects S3 before connecting" "needs a Layr Cloud remote" "$(l wait archived --timeout 1 2>&1)"
has "the timeout is whole seconds" "--timeout takes whole seconds" "$(l wait files --timeout soon 2>&1)"
has "an unrepresentable timeout is rejected" "--timeout is too large" "$(l wait files --timeout 18446744073709551615 2>&1)"
has "one remote at most" "usage: layr wait" "$(l wait a b 2>&1)"
has "the help names the cursor file" "--cursor-file" "$(l wait --help 2>&1)"
finish
