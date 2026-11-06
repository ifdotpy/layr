#!/bin/bash
# ci: macos overlayfs
# `layr usage` counts the commands that run on this machine (src/usage.rs): runs, agent runs and
# failures per command and subcommand. The arguments are never stored.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init u --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/u/lines/main || exit 1
l status >/dev/null; l status >/dev/null
LAYR_SESSION=agent-1 l status >/dev/null
l line new -q secret-line-name
echo x >> notes.txt; l save -qam "secret message"
l show no-such-rev:notes.txt >/dev/null 2>&1
json=$(l usage --json)
field() { python3 -c "import json,sys; rows=[r for r in json.load(sys.stdin) if r['command']==sys.argv[1] and r['sub']==sys.argv[2]]; print(rows[0][sys.argv[3]] if rows else 0)" "$@"; }
eq "status runs are counted" 3 "$(field status '' runs <<<"$json")"
eq "the run with LAYR_SESSION is an agent run" 1 "$(field status '' agent_runs <<<"$json")"
eq "a subcommand is counted with its command" 1 "$(field line new runs <<<"$json")"
eq "a failed command is counted as failed" 1 "$(field show '' failures <<<"$json")"
has "the table names the commands" "status" "$(l usage)"
db=$LAYR_ROOT/machine/usage.sqlite
ok "the count lives in the machine folder" test -f "$db"
no "no argument is stored" sh -c 'cat "$1"* | grep -qa -e secret -e no-such-rev' sh "$db"
no "usage takes no positional argument" l usage status
finish
