#!/bin/bash
# ci: macos
# Check platforms (docs/checks.md): an executor attestation names the platform it ran on
# (`<os>-<arch>`), `checks.platforms` lists the platforms a check must pass on, and `checks pending`
# lists for one platform the checks that no attestation covers, so CI runs only those.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
here=$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/macos/')-$(uname -m | sed 's/arm64/aarch64/')
other=linux-x86_64; [ "$here" = linux-x86_64 ] && other=linux-aarch64
l config check.unit true >/dev/null
l protect main >/dev/null 2>&1 || true
l config checks.lines '{"main":["unit"]}' >/dev/null
l config checks.platforms "{\"unit\":[\"$here\",\"$other\"]}" >/dev/null
count() { python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))'; }

eq "before any run the check is pending here" 1 "$(l checks pending --state main --json | count)"
eq "and on the other platform" 1 "$(l checks pending --state main --platform "$other" --json | count)"
has "a pending job names its platform" "\"platform\":\"$other\"" "$(l checks pending --state main --platform "$other" --json)"

l config --local checks.runner true >/dev/null
out=$(l checks watch --once --json 2>&1)
has "the executor runs the check of this platform only" "\"count\":1" "$out"
has "its attestation names the platform" "\"platform\":\"$here\"" "$out"
eq "then nothing is pending here" 0 "$(l checks pending --state main --json | count)"
eq "the other platform still needs its own run" 1 "$(l checks pending --state main --platform "$other" --json | count)"
has "a second watch runs nothing" "\"count\":0" "$(l checks watch --once --json 2>&1)"

l config checks.platforms "{\"unit\":\"${here%%-*}\"}" >/dev/null
eq "an OS entry takes any architecture" 0 "$(l checks pending --state main --json | count)"
l config checks.platforms '{}' >/dev/null
eq "a check without platforms takes a result from any platform" 0 "$(l checks pending --state main --platform "$other" --json | count)"

no "an unknown platform is refused" l config checks.platforms '{"unit":"windows"}'
no "an empty list is refused" l config checks.platforms '{"unit":[]}'
no "pending refuses an OS without an architecture" l checks pending --platform linux

# macOS runs checks without a sandbox: a state that is not of main and has no approval waits, and the run says why.
if [ "$(uname -s)" = Darwin ]; then
    l line new topic -q
    cd "$LAYR_ROOT/projects/p/lines/topic" || exit 1
    echo t > t.txt; l add t.txt >/dev/null; l save -qm topic
    l protect topic >/dev/null 2>&1 || true
    l config checks.lines '{"main":["unit"],"topic":["unit"]}' >/dev/null
    err=$(l checks run --state topic 2>&1 >/dev/null)
    has "a job that waits says so" "unit of " "$err"
    has "and why" "only a state of main or a state that a maintainer approved runs here" "$err"
fi
finish
