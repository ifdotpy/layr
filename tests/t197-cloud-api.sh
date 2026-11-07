#!/bin/bash
# ci: macos overlayfs
# Layr Cloud API v1 (docs/remotes.md): a Private layr+https remote pushes typed objects, states, records and ref
# moves; a clone on another machine reads them back, pushes, and the first machine fetches without local trust
# (the server access list decides who writes). A ref that moved on the server fails the push until a fetch, a
# missing object fails a clone, and a server that needs a newer client stops the command with the version.
. "$(dirname "$0")/lib.sh"

# Keep the trust anchor separate from the server certificate: TLS implementations differ
# in whether they accept a self-signed end-entity certificate as a root.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$T/tls-ca-key.pem" -out "$T/tls-ca.pem" -days 1 \
    -subj '/CN=Layr test CA' -addext basicConstraints=critical,CA:TRUE \
    -addext keyUsage=critical,keyCertSign,cRLSign >/dev/null 2>&1 || exit 1
openssl req -new -newkey rsa:2048 -nodes -keyout "$T/tls-key.pem" -out "$T/tls.csr" \
    -subj /CN=localhost >/dev/null 2>&1 || exit 1
cat > "$T/tls-ext.cnf" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF
openssl x509 -req -in "$T/tls.csr" -CA "$T/tls-ca.pem" -CAkey "$T/tls-ca-key.pem" -CAcreateserial \
    -out "$T/tls-cert.pem" -days 1 -extfile "$T/tls-ext.cnf" >/dev/null 2>&1 || exit 1
umask 077
echo test.machine.token > "$T/token"
python3 "$(dirname "$0")/fake-cloud-api.py" "$T" test.machine.token > "$T/server.log" 2>&1 &
SERVER=$!
disown
trap 'kill $SERVER 2>/dev/null; cleanup' EXIT
for _ in $(seq 1 100); do
    [ -s "$T/port" ] && break
    kill -0 "$SERVER" 2>/dev/null || break
    sleep 0.1
done
if [ ! -s "$T/port" ]; then
    echo 'FAIL the Cloud test server did not start'
    cat "$T/server.log"
    exit 1
fi
PORT=$(cat "$T/port")
URL="layr+https://localhost:$PORT/p"
export LAYR_GATEWAY_CA_FILE=$T/tls-ca.pem LAYR_GATEWAY_TOKEN_FILE=$T/token
stats() { curl -s --cacert "$T/tls-ca.pem" "https://localhost:$PORT/test/stats"; }
admin() { curl -s --cacert "$T/tls-ca.pem" -X POST -d "$2" "https://localhost:$PORT/test/$1" >/dev/null; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main"
l overlay create private -q
l overlay create shared --readers '*' -q
remote_added=$(l remote add cloud "$URL" 2>&1); remote_rc=$?
eq "remote add of a Layr Cloud project" 0 "$remote_rc"
if [ "$remote_rc" -ne 0 ]; then
    echo "$remote_added"
    exit 1
fi
eq "the mode is Private" private "$(l config --get cloud.remote.cloud.mode)"
l tag before-objects head
mkdir -p portable/deep
printf 'portable history\n' > portable/deep/file
l add portable
l save -m 'portable history' -q
migration=$(l migrate --delete-old 2>&1); migration_rc=$?
eq "migrate before a cloud push" 0 "$migration_rc"
[ "$migration_rc" -eq 0 ] || echo "$migration"
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
new_fs "$LAYR_ROOT" 2G
cd "$T"
ok "clone from Layr Cloud" l clone "$URL" copy -q

eq "the clone reads the files" "$(cat "$T/src/notes.txt")" "$(cat "$LAYR_ROOT/projects/copy/lines/main/notes.txt")"
cd "$LAYR_ROOT/projects/copy/lines/main"
eq "cloud clone includes nested migrated content" "portable history" "$(cat portable/deep/file)"
eq "cloud clone retains migrated history" "$(cat "$T/src/notes.txt")" "$(l show before-objects:notes.txt)"
ok "nested modification times survive cloud transfer" python3 -I -c 'import os,sys; assert os.stat(sys.argv[1]).st_mtime_ns == os.stat(sys.argv[2]).st_mtime_ns' "$FIRST/projects/p/lines/main/portable/deep/file" portable/deep/file
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
