#!/bin/bash
# ci: macos overlayfs
# Layr Cloud API v1 (docs/remotes.md): a Private layr+https remote pushes typed objects, states, records and ref
# moves; a clone on another machine reads them back, pushes, and the first machine fetches without local trust
# (the server access list decides who writes). A ref that moved on the server fails the push until a fetch, a
# missing object fails a clone, and a server that needs a newer client stops the command with the version.
. "$(dirname "$0")/lib.sh"

openssl req -x509 -newkey rsa:2048 -nodes -keyout "$T/tls-key.pem" -out "$T/tls-cert.pem" -days 1 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 >/dev/null 2>&1
umask 077
echo test.machine.token > "$T/token"
python3 "$(dirname "$0")/fake-cloud-api.py" "$T" test.machine.token &
SERVER=$!
disown
trap 'kill $SERVER 2>/dev/null; cleanup' EXIT
for _ in $(seq 1 100); do [ -s "$T/port" ] && break; sleep 0.1; done
PORT=$(cat "$T/port")
URL="layr+https://localhost:$PORT/p"
export LAYR_GATEWAY_CA_FILE=$T/tls-cert.pem LAYR_GATEWAY_TOKEN_FILE=$T/token
stats() { curl -s --cacert "$T/tls-cert.pem" "https://localhost:$PORT/test/stats"; }
admin() { curl -s --cacert "$T/tls-cert.pem" -X POST -d "$2" "https://localhost:$PORT/test/$1" >/dev/null; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l overlay create private -q
l overlay create shared --readers '*' -q
ok "remote add of a Layr Cloud project" l remote add cloud "$URL"
eq "the mode is Private" private "$(l config --get cloud.remote.cloud.mode)"
PUSHED=$(l push cloud 2>&1)
eq "push to Layr Cloud" 0 $?
has "an overlay with limited readers stays here" "overlay private: its readers are limited" "$PUSHED"
has "an overlay that everyone reads goes up" "overlay shared:" "$PUSHED"
eq "the server took states" yes "$(stats | python3 -c 'import json,sys; print("yes" if json.load(sys.stdin)["states"] > 0 else "no")')"
eq "the machine key is registered" 1 "$(stats | python3 -c 'import json,sys; print(json.load(sys.stdin)["machines"])')"
read -r MAIN_ID HEAD < <(l line list --json | python3 -c 'import json,sys; l=next(l for l in json.load(sys.stdin) if l["name"]=="main"); print(l["id"], l["head"])')
eq "the main line ref names the head" "$HEAD" "$(stats | python3 -c "import json,sys; print(json.load(sys.stdin)['refs'].get('lines/$MAIN_ID',''))")"
ok "a second push sends nothing new" l push cloud -q

# Another machine clones the project.
FIRST=$LAYR_ROOT
export LAYR_ROOT=$T/second
cd "$T"
ok "clone from Layr Cloud" l clone "$URL" copy -q

eq "the clone reads the files" "$(cat "$T/src/notes.txt")" "$(cat "$LAYR_ROOT/projects/copy/lines/main/notes.txt")"
cd "$LAYR_ROOT/projects/copy/lines/main"
echo second >> notes.txt
ok "save in the clone" l save -a -m second -q
ok "push from the clone moves the shared line" l push origin -q

export LAYR_ROOT=$FIRST
cd "$LAYR_ROOT/projects/p/lines/main"
ok "fetch takes the other machine's records without local trust" l fetch cloud -q
has "the fetched change is listed" second "$(l change list)"

# The server moved main past this machine's head: the push waits for a merge of what the fetch read.
echo third >> notes.txt
ok "save on the first machine" l save -a -m third -q
has "a push over a line that moved on the server is refused" "merge it into the line" "$(l push cloud 2>&1 || true)"

# Another writer moves the line on the server behind this machine's back.
admin ref "{\"name\":\"lines/$MAIN_ID\",\"target\":\"$HEAD\"}"
export LAYR_ROOT=$T/second
cd "$LAYR_ROOT/projects/copy/lines/main"
echo fourth >> notes.txt
ok "save in the clone again" l save -a -m fourth -q
has "a stale expectation is a conflict that asks for a fetch" "layr fetch" "$(l push origin 2>&1 || true)"
ok "fetch reads the server ref" l fetch origin -q
ok "the push after the fetch moves the line" l push origin -q

# A missing object fails a new clone and names the object.
OBJECT=$(stats | python3 -c 'import json,sys; print([l.split("/")[-1] for l in json.load(sys.stdin)["log"] if l.startswith("PUT /api/v1/projects/p/objects/")][0])')
admin delete "{\"id\":\"$OBJECT\"}"
cd "$T"
has "a clone with a missing object fails" "$OBJECT" "$(l clone "$URL" broken -q 2>&1 || true)"

admin minimum '{"minimum":"99.0.0"}'
cd "$LAYR_ROOT/projects/copy/lines/main"
has "a server that needs a newer client names the version" "needs layr 99.0.0 or newer" "$(l fetch origin 2>&1 || true)"
finish
