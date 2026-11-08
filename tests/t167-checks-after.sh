#!/bin/bash
# ci: macos
# `checks.after` (docs/checks.md): a check runs only after the checks it needs passed on the same state. In one
# pass the needed checks run first. A failed need skips it and a missing result makes it wait, both with no
# attestation; `<check>@<platform>` needs a result of that platform.
. "$(dirname "$0")/lib.sh"
needs_nested_sandbox
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
here=$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/macos/')-$(uname -m | sed 's/arm64/aarch64/')
other=linux-x86_64; [ "$here" = linux-x86_64 ] && other=linux-aarch64
# Each check has its own command: a result counts for the command that it ran.
l config check.build "echo build" >/dev/null
l config check.lint false >/dev/null
l config check.deploy "echo deploy" >/dev/null
l config check.publish "echo publish" >/dev/null
l config check.mirror "echo mirror" >/dev/null
l protect main >/dev/null 2>&1 || true
l config checks.lines '{"main":["deploy","publish","mirror","build","lint"]}' >/dev/null
l config checks.after "{\"deploy\":[\"build\"],\"publish\":[\"lint\"],\"mirror\":[\"build@$other\"]}" >/dev/null
no "an unknown platform in checks.after is refused" l config checks.after '{"deploy":["build@windows"]}'
l config --local checks.runner true >/dev/null
out=$(l checks watch --once --json 2>&1)
names() { python3 -c 'import json,sys
for line in sys.stdin:
    line = line.strip()
    if line.startswith("{"):
        d = json.loads(line)
        print(" ".join(sorted(c["name"] for c in d.get("checks", []))))' ; }
eq "one pass runs the needed checks first, then the check that needs them" "build deploy lint" "$(echo "$out" | names)"
has "a failed need skips the check" "publish of" "$out"
has "the skip says why" "check 'lint' failed" "$out"
has "a need of another platform makes the check wait" "check 'build@$other' has no result yet" "$out"
eq "a second pass runs nothing" "" "$(l checks watch --once --json 2>&1 | names)"
finish
