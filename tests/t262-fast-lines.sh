#!/bin/bash
# ci: macos
# Fast lines (LayrFS, docs/layrfs-lab.md) against this build. tests/fast-parity.sh and
# fast-parity-more.sh run the same commands on a classic and a fast line and compare the output;
# on Linux also fast-sweep.sh (copies go), views-layrfs.sh (the view contract) and fast-upgrade.sh
# (a new daemon takes the lines without an unmount) and fast-crash.sh (a crash or restart of the
# service).
# Linux: a root daemon on the test store serves the views; the parts run as a user with sudo.
# macOS: needs the installed and enabled LayrFS extension; the parts run as the caller.
if [ "$(uname -s)" = Darwin ] && ! pluginkit -m -p com.apple.fskit.fsmodule 2>/dev/null | grep -q dev.layr.fshost.fs; then
    echo "t262-fast-lines.sh (needs the LayrFS extension): 0 passed, 0 failed"
    exit 0
fi
if [ "$(uname -s)" = Linux ] && [ ! -c /dev/fuse ]; then
    echo "t262-fast-lines.sh (needs /dev/fuse): 0 passed, 0 failed"
    exit 0
fi
. "$(dirname "$0")/lib.sh"

# The parts read their scripts as another user: a copy in the test folder.
mkdir -p "$T/parts"
cp "$(dirname "$0")"/fast-parity.sh "$(dirname "$0")"/fast-parity-more.sh "$(dirname "$0")"/fast-sweep.sh \
    "$(dirname "$0")"/fast-upgrade.sh "$(dirname "$0")"/views-layrfs.sh "$(dirname "$0")"/fast-crash.sh \
    "$(dirname "$0")"/fast-remount-macos.sh "$T/parts/"
chmod -R a+rX "$T/parts"

part() { # part <name> <command...>: passes when the command exits 0 and prints a PASS line
    local name=$1 out rc
    shift
    out=$("$@" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -q '^PASS'; then
        PASS=$((PASS + 1))
        echo "PASS $name"
    else
        FAIL=$((FAIL + 1))
        printf '%s\n' "$out" | tail -60
        echo "FAIL $name (exit $rc)"
    fi
}

if [ "$(uname -s)" = Darwin ]; then
    # FSKit does not mount on a disk image or an external volume (the test volume): the parts use
    # a store on the system volume.
    fast_root=$(mktemp -d "/private/tmp/layr-fast-store.XXXXXX")
    export LAYR_ROOT=$fast_root/store TMPDIR=$fast_root/tmp
    mkdir -p "$TMPDIR"
    fast_cleanup() {
        mount | awk -v t="$fast_root/" '/\(layrfs,/ && index($3, t) == 1 { print $3 }' | while IFS= read -r mp; do
            umount "$mp" 2>/dev/null || umount -f "$mp"
        done
        # Sealed states carry the immutable flag or a deny ACL entry (lib.sh cleanup).
        chflags -R nouchg "$fast_root" 2>/dev/null
        find "$fast_root" ! -type p ! -type s -exec /bin/chmod -N {} + 2>/dev/null
        chmod -R u+w "$fast_root" 2>/dev/null
        rm -rf "$fast_root"
    }
    trap 'fast_cleanup; cleanup' EXIT
    part "fast lines match classic lines" bash "$T/parts/fast-parity.sh"
    part "fast lines match classic lines (remotes, caches, checks, queue)" bash "$T/parts/fast-parity-more.sh"
    part "a fast line comes back after its mount is gone" bash "$T/parts/fast-remount-macos.sh"
    finish
    exit $?
fi

u=layr-fast
id "$u" >/dev/null 2>&1 || useradd -m "$u"
sudoers=/etc/sudoers.d/layr-fast-test
printf '%s ALL=(ALL) NOPASSWD: ALL\n' "$u" > "$sudoers"
chmod 440 "$sudoers"
chmod 755 "$T" "$LAYR_ROOT"
cp "$LAYR" "$T/layr" && chmod 755 "$T/layr"
export LAYR_SOCKET=$T/layr.sock
"$T/layr" daemon --socket "$LAYR_SOCKET" > "$T/daemon.log" 2>&1 &
stop() {
    # fast-upgrade.sh starts a new daemon on the same socket: stop every one.
    pkill -f "layr daemon --socket $LAYR_SOCKET" 2>/dev/null
    for _ in $(seq 1 50); do pgrep -f "layr daemon --socket $LAYR_SOCKET" >/dev/null || break; sleep 0.1; done
    findmnt -rn -o TARGET | awk -v t="$T/" 'index($0, t) == 1' | sort -r | while IFS= read -r mp; do umount -l "$mp"; done
    rm -f "$sudoers"
}
trap 'if [ "$FAIL" -ne 0 ]; then tail -40 "$T/daemon.log"; tail -20 "$T/tmp/daemon-new.log" 2>/dev/null; fi; stop; cleanup' EXIT
for _ in $(seq 1 100); do [ -S "$LAYR_SOCKET" ] && break; sleep 0.1; done
mkdir -p "$T/tmp"
chown "$u" "$T/tmp"
as() {
    runuser -u "$u" -- env LAYR="$T/layr" LAYR_ROOT="$LAYR_ROOT" LAYR_SOCKET="$LAYR_SOCKET" PATH="$PATH" \
        HOME="$(getent passwd "$u" | cut -d: -f6)" TMPDIR="$T/tmp" DAEMON_LOG="$T/tmp/daemon-new.log" "$@"
}
cd "$T/tmp"
part "fast lines match classic lines" as bash "$T/parts/fast-parity.sh"
part "fast lines match classic lines (remotes, caches, checks, queue)" as bash "$T/parts/fast-parity-more.sh"
# The sweep part reads the root-owned store folders with sudo. A check runs with "no new privileges", where sudo
# cannot run (2026-11-22): the part runs on a developer machine, not in a check.
if grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status 2>/dev/null; then
    echo "SKIP copies of fast lines go (sudo cannot run with no new privileges, as in a check)"
else
    part "copies of fast lines go" as bash "$T/parts/fast-sweep.sh"
fi
part "the view contract over LayrFS" as bash "$T/parts/views-layrfs.sh"
part "a new daemon takes the fast lines" as bash "$T/parts/fast-upgrade.sh"
part "fast lines come back after a crash and a restart" as bash "$T/parts/fast-crash.sh"
cd /
finish
