#!/bin/bash
# LayrFS lines on Linux: a handoff that the taker does not confirm leaves the line with its server.
# A fake taker asks for the line, takes the view state and the /dev/fuse descriptor, then goes
# away without its `r` (ready) frame, or answers with another frame. The old server must serve the
# line again: reads, writes and the control socket work.
# Provide LAYR (built with the feature layrfs-fuse), an isolated LAYR_ROOT, and LAYR_SOCKET of a
# root daemon for that store. Run as a non-root user with sudo (the taker must be the server's user).
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run this test as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}" "${LAYR_SOCKET:?}"
export LAYR_AUTHOR_NAME=Handoff LAYR_AUTHOR_EMAIL=handoff@example.invalid GIT_CONFIG_GLOBAL=/dev/null
work=$(mktemp -d "${TMPDIR:-/tmp}/layr-handoff.XXXXXX")
p="handoff-$(date +%s)-$$"
mkdir -p "$work/s"; printf 'one\n' > "$work/s/a"
"$LAYR" init "$p" --from "$work/s" -q
cd "$LAYR_ROOT/projects/$p/lines/main"
"$LAYR" protect main --remove
"$LAYR" config line.kind fast
"$LAYR" migrate --delete-old >/dev/null
"$LAYR" line new x --fast >/dev/null
V="$LAYR_ROOT/projects/$p/lines/x"
printf 'two\n' > "$V/a"
id=$(sudo ls "$LAYR_ROOT/projects/$p/meta/vlines" | head -1)
sock="$(dirname "$LAYR_SOCKET")/views/$id.sock"
sudo test -S "$sock" || { echo "FAIL: no control socket $sock" >&2; exit 1; }
cat > "$work/taker.py" <<'PY'
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
req = json.dumps({"op": "handoff", "ack": True}).encode()
s.sendall(b"q" + struct.pack(">I", len(req)) + req)
def exact(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            sys.exit("closed")
        b += c
    return b
head = exact(5)
exact(struct.unpack(">I", head[1:])[0])
msg, fds, _, _ = socket.recv_fds(s, 1, 4)
print(chr(head[0]), msg.decode(), len(fds))
if sys.argv[2] == "wrong":
    s.sendall(b"x" + struct.pack(">I", 0))
s.close()
PY
want=two
for mode in drop wrong; do
  got=$(sudo python3 "$work/taker.py" "$sock" $mode)
  [ "$got" = "h a 1" ] || { echo "FAIL ($mode): the server sent '$got', not the state, byte a and one descriptor" >&2; exit 1; }
  [ "$(cat "$V/a")" = "$want" ] || { echo "FAIL ($mode): no read after the handoff" >&2; exit 1; }
  printf '%s\n' "$mode" > "$V/a"
  [ "$(cat "$V/a")" = "$mode" ] || { echo "FAIL ($mode): no write after the handoff" >&2; exit 1; }
  want=$mode
  (cd "$V" && "$LAYR" status --porcelain) | grep -q '^ M a$' || { echo "FAIL ($mode): no status after the handoff" >&2; exit 1; }
done
cd "$LAYR_ROOT/projects/$p/lines/main"
"$LAYR" line delete -D --discard x >/dev/null
rm -rf "$work"
echo 'PASS: an unconfirmed handoff leaves the line with its server'
