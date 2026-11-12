#!/bin/bash
# The command line: the global --line names the line of a command (log too), `--` ends the
# options, a value option takes a next value that starts with '-', and `<command> --help` prints
# the help of that command. One user, so it runs on macOS and on Linux.
. "$(dirname "$0")/lib.sh"

sample $T/src
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
l -C $P/main line new work main >/dev/null
cd $P/work

# 1. A message that starts with '-' is the value of -m, also "-h" and "--help".
echo one > one.txt
l add one.txt
ok "save -m takes a message that starts with -" l save -m "-x first" -q
eq "the message is kept" "-x first" "$(l log -1 --format=%s)"
echo two >> one.txt
ok "save -m takes '-- looks good'" l save -a -m "-- looks good" -q
eq "the second message is kept" "-- looks good" "$(l log -1 --format=%s)"
echo three >> one.txt
out=$(l save -a -m -h -q 2>&1)
eq "save -m -h saves and prints no help" "" "$out"
eq "the message -h is kept" "-h" "$(l log -1 --format=%s)"

# 2. `--` ends the options: a comment that starts with '-'.
CHANGE=$(l change list --json | python3 -c 'import json,sys; print(next(c["change_id"] for c in json.load(sys.stdin) if c["message"] == "-x first"))')
out=$(l comment "$CHANGE" "-1 for this" 2>&1)
has "text that starts with - without -- is refused with the fix" "put -- before" "$out"
ok "comment takes text after --" l comment "$CHANGE" -- "-1 for this"
ok "comment takes '-- looks good' after --" l comment "$CHANGE" --json -- "-- looks good"
texts=$(l change show "$CHANGE" --json | python3 -c 'import json,sys; print("|".join(c["text"] for c in json.load(sys.stdin)["comments"]))')
eq "the change holds both comments as written" "-1 for this|-- looks good" "$texts"

# 3. The global --line names the line, for log as for status.
cd $P/main
eq "--line selects the line of log" "$(l log work --oneline)" "$(l --line work log --oneline)"
has "--line work log shows the states of work" "-x first" "$(l --line work log --oneline)"
eq "--line and the same line in log agree" "$(l log work --oneline)" "$(l --line work log work --oneline)"
out=$(l --line work log main 2>&1)
rc=$?
eq "--line and another line in log are refused" 128 "$rc"
has "the refusal names both lines" "--line work and 'log main' name two lines" "$out"
has "--line selects the line of status" "On line work" "$(l --line work status)"
has "without --line, status reads the line of the folder" "On line main" "$(l status)"

# 4. The help of one command, from any folder.
has "checks --help prints the usage of checks" "usage: layr checks watch" "$(l checks --help)"
eq "help comment is comment --help" "$(l comment --help)" "$(l help comment)"
has "log --help names log" "usage: layr log" "$(l log --help)"
no "log --help is not the general help" test "$(l log --help)" = "$(l help)"

finish
