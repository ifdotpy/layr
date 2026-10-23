#!/bin/bash
# Regressions: streams outside their project, big files, tags, sync trust and the service client.
. "$(dirname "$0")/lib.sh"

chmod 755 $T $LAYR_ROOT
for u in layr-admin layr-u1; do id $u >/dev/null 2>&1 || useradd -m $u; done
export LAYR_SOCKET=$T/layr.sock
cp $LAYR $T/layr && chmod 755 $T/layr
$T/layr daemon --socket $LAYR_SOCKET --tick 60 > $T/daemon.log 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; cleanup' EXIT
for i in $(seq 1 50); do [ -S $LAYR_SOCKET ] && break; sleep 0.1; done
as() { local u=$1; shift; runuser -u $u -- env LAYR_SOCKET=$LAYR_SOCKET PATH=/usr/bin:/bin "$@"; }
sample $T/src
l init s --from $T/src --owner layr-admin -q >/dev/null
P=$LAYR_ROOT/projects/s/lines
cd $P/main || exit 1
echo two >> notes.txt; l commit -qam two
# The state folder of a revision (rev-parse prints the display form of the id).
state_dir() { ls -d $LAYR_ROOT/projects/$1/states/*-$(l --project $1 --line main rev-parse $2 | cut -c1-12); }
S1=$(basename $(state_dir s HEAD~1))

# 3. A stream cannot take its parent from another project.
mkdir -p $T/vsrc; echo TOPSECRET > $T/vsrc/key.txt
l init v --from $T/vsrc -q >/dev/null
V=$(state_dir v HEAD)
btrfs -q subvolume snapshot $V $LAYR_ROOT/x-work; echo x > $LAYR_ROOT/x-work/x.txt
mkdir $LAYR_ROOT/x-ro; btrfs -q subvolume snapshot -r $LAYR_ROOT/x-work $LAYR_ROOT/x-ro/X
btrfs send -q -p $V $LAYR_ROOT/x-ro/X > $T/foreign.stream
U=$(btrfs subvolume show $LAYR_ROOT/x-ro/X | awk '$1 == "UUID:" {print $2}')
out=$(l debug receive X $U < $T/foreign.stream 2>&1); rc=$?
eq "a stream with a parent in another project is refused" "128" "$rc"
has "with the reason" "not a state of this project" "$out"
eq "and leaves nothing" "" "$(ls $LAYR_ROOT/projects/s/states | grep -x X)"
btrfs send -q $LAYR_ROOT/x-ro/X > $T/full.stream
has "a stream named for another subvolume is refused" "not the subvolume its record names" \
    "$(l debug receive X 00000000-0000-0000-0000-000000000000 < $T/full.stream 2>&1)"

# 4. Big files are never read whole: diff, show, merge, grep and blame stay within 1 GB of memory.
truncate -s 2G big.bin; l add big.bin; l commit -qm big
l branch -q b1
cd $P/b1 && printf 'theirs' | dd of=big.bin bs=1 seek=100 conv=notrunc 2>/dev/null && l commit -qam b1 && cd $P/main
printf 'ours' | dd of=big.bin bs=1 seek=5000 conv=notrunc 2>/dev/null; l commit -qam ours
mem() { ( ulimit -v 1000000; "$@" ); }
eq "diff --stat of a 2 GiB file" " big.bin | Bin 0 -> 2147483648 bytes" "$(mem $LAYR diff --stat HEAD~2 HEAD~1 | head -1)"
has "diff says binary" "Binary files /dev/null and b/big.bin differ" "$(mem $LAYR diff HEAD~2 HEAD~1)"
eq "show streams the file" "2147483648" "$(mem $LAYR show HEAD:big.bin | wc -c)"
eq "merge of a big file is a binary conflict" "1" "$(mem $LAYR merge -q b1 >/dev/null 2>&1; echo $?)"
has "listed as a conflict" "big.bin" "$(l status -s)"
eq "grep skips it" "1" "$(mem $LAYR grep -q theirs; echo $?)"
has "blame refuses it" "larger than 64 MiB" "$(mem $LAYR blame big.bin 2>&1)"

# 5. Tags are set by the admin.
l branch -q t1 --owner layr-u1
out=$(cd $P/t1 && as layr-u1 $T/layr tag v-member 2>&1); rc=$?
eq "a member cannot create a tag" "128" "$rc"
has "with the reason" "needs the role maintainer" "$out"
eq "no tag" "" "$(l tag -l v-member)"
ok "the admin can" sh -c "cd $P/main && runuser -u layr-admin -- env LAYR_SOCKET=$LAYR_SOCKET $T/layr tag v-admin"
eq "the admin's tag" "v-admin" "$(l tag -l v-admin)"

# 6. Sync trusts the peer only after its records passed the checks.
cat > $T/fake-peer.py <<'PY'
import json, struct, sys
r, w = sys.stdin.buffer, sys.stdout.buffer
def recv():
    h = r.read(5)
    return json.loads(r.read(struct.unpack(">I", h[1:])[0]))
def send(v):
    d = json.dumps(v).encode()
    w.write(b"J" + struct.pack(">I", len(d)) + d)
    w.flush()
hello = recv()
send({"op": "hello", "project_id": hello["project_id"], "machine": "0190f0aa-0000-7000-8000-00000000f00d",
      "public_key": "11" * 32, "seqs": {}, "present": [], "caches": []})
while recv().get("more"):
    pass
send({"op": "records", "lines": ["{\"r\": {}, \"s\": \"\"}"], "more": False})
r.read()
PY
no "a sync whose peer records fail" peer_exchange sync s -q -- python3 $T/fake-peer.py
eq "leaves the peer untrusted" "0" "$(python3 -c "import sqlite3; print(sqlite3.connect('$LAYR_ROOT/projects/s/meta/db/layr.sqlite').execute(\"SELECT count(*) FROM trusted WHERE machine LIKE '%f00d'\").fetchone()[0])")"

# 7. The client reports a request it could not send. The fake service never reads it and
# closes after a second; the request (1.5 MB of environment) does not fit the socket buffer.
python3 - $T/fake.sock > /dev/null 2>&1 <<'PY' &
import os, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); os.chmod(sys.argv[1], 0o777); s.listen(1)
s.settimeout(30)
c, _ = s.accept(); time.sleep(1); c.close()
PY
for i in $(seq 1 50); do [ -S $T/fake.sock ] && break; sleep 0.1; done
big=$(head -c 100000 /dev/zero | tr '\0' x)
envs=(); for i in $(seq 1 15); do envs+=("V$i=$big"); done
out=$(cd /tmp && runuser -u layr-u1 -- env LAYR_SOCKET=$T/fake.sock "${envs[@]}" $T/layr status 2>&1)
has "a failed send is reported" "cannot send the request" "$out"
finish
