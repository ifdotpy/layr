#!/bin/bash
# ci: macos overlayfs
# A platform change during a check must not let the old result move main.
. "$(dirname "$0")/lib.sh"
set -e
sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
cat > "$T/gate.sh" <<EOF
#!/bin/sh
touch '$T/started'
for n in \$(seq 1 300); do
    [ -f '$T/release' ] && exit 0
    sleep 0.1
done
exit 1
EOF
l config check.gate "sh $T/gate.sh" >/dev/null
l protect main --check gate >/dev/null
base=$(l rev-parse head)
l line new -q topic
cd ../topic
echo change > change.txt
l add change.txt
l save -qm change
l queue add head >/dev/null
cd ../main
l queue run --into main > "$T/result" 2>&1 &
job=$!
started=false
for n in $(seq 1 300); do
    if [ -f "$T/started" ]; then started=true; break; fi
    kill -0 "$job" 2>/dev/null || break
    sleep 0.1
done
if [ "$started" != true ]; then
    touch "$T/release"
    wait "$job" || true
    echo 'FAIL check did not start'
    cat "$T/result"
    exit 1
fi
l config checks.platforms '{"gate":"linux-aarch64"}' >/dev/null
touch "$T/release"
wait "$job" || true
eq 'a changed platform leaves main unchanged' "$base" "$(l rev-parse head)"
has 'the queue reports the changed platform' 'platforms changed during checks' "$(cat "$T/result")"
finish
