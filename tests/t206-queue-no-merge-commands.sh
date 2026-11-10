#!/bin/bash
# ci: macos overlayfs
# Candidate preparation must not execute commands configured for merge drivers or derive rules.
. "$(dirname "$0")/lib.sh"
mkdir -p "$T/src"
printf 'num.txt merge=evil\n' > "$T/src/.layrattributes"
echo base > "$T/src/num.txt"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l config merge.driver.evil "touch $T/driver-ran; echo merged > %A" >/dev/null
l line new -q topic
cd ../topic
echo source > num.txt; l save -qam source
state=$(l rev-parse head)
entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
echo target > num.txt; l save -qam target
head=$(l rev-parse head)
no 'a candidate that needs a custom driver is not prepared' l queue candidate "$entry" --expect-head "$head" --expect-state "$state"
no 'candidate preparation does not run the custom driver' test -e "$T/driver-ran"
eq 'main stays unchanged' "$head" "$(l rev-parse head)"
eq 'the queue entry stays pending' pending "$(l queue list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"])')"

mkdir -p "$T/verify"
printf 'num.txt merge=blocks merge-verify=evil\n' > "$T/verify/.layrattributes"
python3 -c 'open("'"$T"'/verify/num.txt","wb").write(bytes(65536))'
l init v --from "$T/verify" -q >/dev/null
cd "$LAYR_ROOT/projects/v/lines/main"
l config merge.verify.evil "touch $T/verify-ran; true" >/dev/null
l line new -q topic
cd ../topic
python3 -c 'p="num.txt"; f=open(p,"r+b"); f.seek(0); f.write(b"source"); f.close()'; l save -qam source
state=$(l rev-parse head)
entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
python3 -c 'p="num.txt"; f=open(p,"r+b"); f.seek(40960); f.write(b"target"); f.close()'; l save -qam target
head=$(l rev-parse head)
no 'a candidate that needs a verify command is not prepared' l queue candidate "$entry" --expect-head "$head" --expect-state "$state"
no 'candidate preparation does not run the verify command' test -e "$T/verify-ran"

mkdir -p "$T/derived"
printf 'first\nsecond\nthird\n' > "$T/derived/input"
echo original > "$T/derived/middle"
printf 'middle derive=evil\n' > "$T/derived/.layrattributes"
l init d --from "$T/derived" -q >/dev/null
cd "$LAYR_ROOT/projects/d/lines/main"
l config derive.command.evil "touch $T/derive-ran; cat input > middle" >/dev/null
l config derive.inputs.evil input >/dev/null
l line new -q topic
cd ../topic
python3 -c 'p="input"; s=open(p).read(); open(p,"w").write(s.replace("third","source"))'; echo source > middle; l save -qam source
state=$(l rev-parse head)
entry=$(l queue add head --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
cd ../main
python3 -c 'p="input"; s=open(p).read(); open(p,"w").write(s.replace("first","target"))'; echo target > middle; l save -qam target
head=$(l rev-parse head)
no 'a candidate that needs a derive command is not prepared' l queue candidate "$entry" --expect-head "$head" --expect-state "$state"
no 'candidate preparation does not run the derive command' test -e "$T/derive-ran"
eq 'the derived merge leaves main unchanged' "$head" "$(l rev-parse head)"
finish
