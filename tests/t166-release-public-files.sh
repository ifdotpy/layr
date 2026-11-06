#!/bin/bash
# ci: macos
# `release attach --public-files` also writes each file as plaintext at public/release/<version>/<name> and then
# the list public/release/<version>/files.json (name, size, SHA-256), create-only: the gateway serves them without
# sign-in. The signed release record keeps the same digests. A second attach of the same bytes succeeds. One user.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l protect main --remove >/dev/null 2>&1 || true
l remote add origin "file://$T/archive" --paper-key-out "$T/paper" >/dev/null 2>&1
l push origin -q
head -c 300000 /dev/urandom > "$T/layr-v1.0.0-linux-x86_64.tar.gz"
echo built > "$T/notes.txt"
l ref set release/v1.0.0 main >/dev/null
ok "attach with public files" l release attach release/v1.0.0 "$T/layr-v1.0.0-linux-x86_64.tar.gz" "$T/notes.txt" --public-files
pub=$(find "$T/archive" -path '*public/release/v1.0.0' -type d | head -1)
ok "the public folder is in the store" test -n "$pub"
ok "a public file is the plaintext" cmp "$T/layr-v1.0.0-linux-x86_64.tar.gz" "$pub/layr-v1.0.0-linux-x86_64.tar.gz"
sha=$(shasum -a 256 "$T/layr-v1.0.0-linux-x86_64.tar.gz" | cut -d' ' -f1)
eq "files.json names its SHA-256" "$sha" "$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print([f["sha256"] for f in d["files"] if f["name"].endswith(".tar.gz")][0])' "$pub/files.json")"
eq "files.json names the version" v1.0.0 "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$pub/files.json")"
eq "the signed record has the same digest" "$sha" "$(l release files release/v1.0.0 --json | python3 -c 'import json,sys; print([f["sha256"] for f in json.load(sys.stdin) if f["name"].endswith(".tar.gz")][0])')"
ok "a second attach of the same bytes succeeds" l release attach release/v1.0.0 "$T/layr-v1.0.0-linux-x86_64.tar.gz" "$T/notes.txt" --public-files
echo other > "$T/notes.txt"
no "other bytes under a published name are refused" l release attach release/v1.0.0 "$T/notes.txt" --public-files
eq "the public file keeps its bytes" built "$(cat "$pub/notes.txt")"
finish
