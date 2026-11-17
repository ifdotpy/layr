#!/bin/bash
# ci: macos macos-plain overlayfs
# A check selects and records its execution location, not its staging directory or source line.
. "$(dirname "$0")/lib.sh"
needs_nested_sandbox
if [ "$(uname -s)" = Linux ] && [ "$(id -u)" != 0 ]; then
    echo "$(basename "$0") (needs an isolated root daemon fixture on Linux): 0 passed, 0 failed"
    exit 0
fi

sample "$T/src"
echo 'target regenerable cache-location-bound' > "$T/src/.layrattributes"
l init checkloc --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/checkloc/lines/main"
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target
echo foreign > target/foreign
l save --overlays -q
l protect main >/dev/null
l config check.a 'test ! -e target/foreign && mkdir -p target && pwd > target/built-at' >/dev/null
l config check.b 'if [ "$(uname -s)" = Linux ]; then test "$(cat target/built-at)" = /work || exit 1; else test ! -e target/built-at || exit 1; fi; mkdir -p target; pwd > target/built-at' >/dev/null
# Job names do not determine execution order; b must consume the output saved back by a.
l config checks.after '{"b":["a"]}' >/dev/null
l config checks.lines '{"main":["a","b"]}' >/dev/null
if [ "$(id -u)" = 0 ]; then
    # The isolated root fixture enables its runner; a non-root service user invokes the checks.
    id layr-check-location >/dev/null 2>&1 || useradd -m layr-check-location
    l access grant layr-check-location reader >/dev/null
    chmod 755 "$T"
    cp "$LAYR" "$T/layr"
    check_socket=$T/check.sock
    "$T/layr" daemon --socket "$check_socket" --tick 3600 >"$T/daemon.log" 2>&1 &
    daemon_pid=$!
    trap 'kill "$daemon_pid" 2>/dev/null || true; wait "$daemon_pid" 2>/dev/null || true; cleanup' EXIT
    for i in $(seq 1 100); do [ -S "$check_socket" ] && break; sleep .1; done
    l config --local checks.runner true >/dev/null
    out=$(cd / && runuser -u layr-check-location -- env LAYR_DIRECT=0 LAYR_SOCKET="$check_socket" "$T/layr" --project checkloc checks run --state main --json 2>"$T/check.err")
else
    l config --local checks.runner true >/dev/null
    out=$(l checks run --state main --json 2>"$T/check.err")
fi
has "both checks ran" '"count":2' "$out"
printf 'check results: %s\n' "$out"
[ ! -s "$T/check.err" ] || cat "$T/check.err"
python3 - "$LAYR_ROOT/projects/checkloc/states" "$out" <<'PY'
import json, pathlib, sys
for check in json.loads(sys.argv[2]).get('checks', []):
    if check.get('exit') != 0:
        log = pathlib.Path(sys.argv[1]) / check['output_state'] / 'check.log'
        print(f"{check['name']} exit {check.get('exit')}:\n{log.read_text(errors='replace')}")
PY
no "the check rejects line output and reuses only the same execution location" grep -Eq '"exit":[1-9]|"exit":null' <<<"$out"
eq "checks leave the source line's output intact" foreign "$(cat target/foreign)"
# Verify the saved state provenance rather than assuming that a passing script implies a correct key.
python3 - "$LAYR_ROOT/projects/checkloc/meta/overlays/$TG/meta/db/layr.sqlite" "$(uname -s)" <<'PY'
import json, sqlite3, sys
locations = []
for text, in sqlite3.connect(sys.argv[1]).execute("SELECT rec FROM states"):
    rec = json.loads(text)
    value = rec.get("cache_key_parts", {}).get("location")
    if value:
        locations.append(json.loads(value)["location"])
checks = [p for p in locations if p["workspace"] == "/work" or "layr-check-" in p["workspace"]]
assert checks, locations
for p in checks:
    assert p["target"] == p["workspace"] + "/target", p
    assert (p["workspace"] == "/work") == (sys.argv[2] == "Linux"), p
PY
eq "saved caches record the actual execution location" 0 "$?"
finish
