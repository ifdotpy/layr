# Test helpers. Each test file gets a fresh btrfs file system on a loop file.
# Needs root, btrfs-progs, git, zstd. LAYR points at the binary under test.

set -u
LAYR=${LAYR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/layr}
PASS=0
FAIL=0
if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ]; then
    T=$(mktemp -d "$LAYR_OVERLAYFS_TEST_ROOT/layr-test.XXXXXX")
elif [ "$(uname -s)" = Darwin ]; then
    : "${LAYR_APFS_TEST_ROOT:?Set LAYR_APFS_TEST_ROOT to a case-sensitive APFS volume}"
    T=$(mktemp -d "$LAYR_APFS_TEST_ROOT/layr-test.XXXXXX")
    export LAYR_TEST_DIRECT_MAIN=1
else
    T=$(mktemp -d "${TMPDIR:-/var/tmp}/layr-test.XXXXXX")
fi
# Stop before any delete: without the folder, "$T/src" would become "/src".
[ -n "$T" ] && [ -d "$T" ] || { echo "FATAL: cannot create the test folder" >&2; exit 1; }
# layr prints the canonical path (/private/var on macOS): compare with it.
T=$(cd "$T" && pwd -P)
export LAYR_ROOT=$T/store
# Tests empty the trash in the command itself (bounded), so their cleanup does not race the
# background emptier; t82 checks the background path.
export LAYR_TRASH_BUDGET_MS=${LAYR_TRASH_BUDGET_MS:-2000}
# A sweep releases a dead chunk at its first pass: one machine pushes at a time here (t148 tests the wait).
export LAYR_TEST_CHUNK_GRACE=${LAYR_TEST_CHUNK_GRACE:-0}
export GIT_AUTHOR_NAME=Tester GIT_AUTHOR_EMAIL=tester@example.com GIT_COMMITTER_NAME=Tester GIT_COMMITTER_EMAIL=tester@example.com
export LAYR_AUTHOR_NAME=Tester LAYR_AUTHOR_EMAIL=tester@example.com
# The platform key of this machine as layr names it (`<os>-<arch>`, docs/overlays.md section 1a,
# slice 3). A cache is the glass of one platform: `glass target` names the glass of the folder
# `target` (`target.macos-aarch64`), and `TG` is that name.
HERE_PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]' | sed 's/^darwin$/macos/')-$(uname -m | sed 's/^arm64$/aarch64/')
glass() { local f=${1//\//.}; echo "$f.${LAYR_TEST_PLATFORM:-$HERE_PLATFORM}"; }
TG=$(glass target)
unset LAYR_PROJECT LAYR_LINE LAYR_SOCKET

# Resolve rustup's toolchain in the sudo user's home, where the test owner can read it.
owner_toolchain_env() {
    if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
        local sudo_home
        sudo_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
        export RUSTUP_HOME=${SUDO_RUSTUP_HOME:-$sudo_home/.rustup} CARGO_HOME=${SUDO_CARGO_HOME:-$sudo_home/.cargo}
    fi
}

# owner_cargo <user>: on a root run, the machine's cargo for a test owner, in the owner's ~/.cargo/bin
# (on the PATH of its cache hooks). The same content for every test, written atomically and kept, so
# tests that run at once share it.
owner_cargo() {
    local user=$1 home real
    home=$(getent passwd "$user" | cut -d: -f6)
    real=$(rustup which cargo 2>/dev/null || command -v cargo || true)
    [ -n "$home" ] && [ -n "$real" ] || { echo "FATAL: no cargo for test owner $user" >&2; exit 1; }
    install -d -o "$user" "$home/.cargo" "$home/.cargo/bin"
    printf '#!/bin/sh\nexport PATH=%s:/usr/bin:/bin\nexport CARGO_HOME=%s\nexec %s "$@"\n' \
        "$(dirname "$real")" "$home/.cargo" "$real" > "$home/.cargo/bin/cargo.$$"
    chmod 755 "$home/.cargo/bin/cargo.$$"
    mv -f "$home/.cargo/bin/cargo.$$" "$home/.cargo/bin/cargo"
}
# Inside a macOS check sandbox (tools/checks/apfs.sh): the executor sets LAYR_CHECK_TMPDIR.
in_check_sandbox() { [ -n "${LAYR_CHECK_TMPDIR:-}" ]; }
# skip_in_check_sandbox <why>: the sandbox forbids what this test needs; ci.yml's macos job runs it.
skip_in_check_sandbox() {
    in_check_sandbox || return 0
    echo "$(basename "$0") (skipped in a check sandbox: $1): 0 passed, 0 failed"
    exit 0
}
# A check sandbox may refuse a nested sandbox (sandbox_apply EPERM, as on macOS 27).
needs_nested_sandbox() {
    in_check_sandbox || return 0
    /usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true 2>/dev/null ||
        skip_in_check_sandbox "it runs a nested check sandbox"
}

l() {
    "$LAYR" "$@"
    local rc=$?
    # The Linux suite runs as root and bypasses policy. On macOS the owner explicitly
    # permits direct updates for these command tests. Access tests use their own policy.
    if [ "$rc" -eq 0 ] && [ "${LAYR_TEST_DIRECT_MAIN:-}" = 1 ]; then
        local project=""
        case "${1:-}" in init) project=$2 ;; git) [ "${2:-}" = clone ] && project=${4:-} ;; esac
        if [ -n "$project" ]; then
            (cd "$LAYR_ROOT/projects/$project/lines/main" && "$LAYR" protect main --remove) || return $?
        fi
    fi
    return "$rc"
}

new_fs() { # new_fs <mountpoint> <size>
    local mp=$1 size=${2:-8G}
    if [ -n "${LAYR_OVERLAYFS_TEST_ROOT:-}" ] || [ "$(uname -s)" = Darwin ]; then
        mkdir -p "$mp"
        return
    fi
    truncate -s "$size" "$mp.img" || exit 1
    mkfs.btrfs -q -f "$mp.img" || exit 1
    mkdir -p "$mp" || exit 1
    for attempt in $(seq 1 50); do
        if mount -o loop,user_subvol_rm_allowed "$mp.img" "$mp" 2> "$mp.mount-error"; then
            rm "$mp.mount-error"
            return
        fi
        sleep 0.1
    done
    cat "$mp.mount-error" >&2
    echo "FATAL: cannot mount the test filesystem at $mp" >&2
    exit 1
}

# A failed cd must stop the test file: a later relative command (rm -rf ./*, layr commands)
# would otherwise run in the previous folder, which can be / after cleanup steps.
cd() {
    builtin cd "$@" || {
        echo "FATAL: cd $* failed; stopping this test file" >&2
        exit 1
    }
}

cleanup() {
    # Never act on an empty or missing test root: "^$T" would match every mount.
    if [ -z "${T:-}" ] || [ ! -d "$T" ] || [ "$T" = / ]; then
        return
    fi
    cd /
    if [ "$(uname -s)" = Darwin ]; then
        # Sealed entries carry the immutable flag (the root, an older store) or a deny ACL entry.
        chflags -R nouchg "$T" 2>/dev/null || true
        /bin/chmod -R -N "$T" 2>/dev/null || true
        chmod -R u+w "$T" 2>/dev/null || true
        rm -rf "$T"
        return
    fi
    if [ "${LAYR_TEST_BTRFS_CLEANUP:-}" = 1 ]; then
        # Only an isolated test folder directly under TMPDIR may use this path.
        local base name private trash mounts contents
        base=$(cd "${TMPDIR:?}" && pwd -P) || exit 1
        case "$T" in "$base"/layr-test.*) name=${T#"$base"/} ;; *) echo "FATAL: test folder is outside TMPDIR: $T" >&2; exit 1 ;; esac
        case "$name" in */*) echo "FATAL: invalid test folder: $T" >&2; exit 1 ;; esac
        mounts=$(findmnt -J -o TARGET) || { echo 'FATAL: cannot inspect test mounts' >&2; exit 1; }
        if ! printf '%s\n' "$mounts" | python3 -c '
import json, sys
root = sys.argv[1]
def check(items):
    for item in items:
        target = item["target"]
        if target == root or target.startswith(root + "/") or check(item.get("children", [])):
            return True
    return False
sys.exit(1 if check(json.load(sys.stdin)["filesystems"]) else 0)
' "$T"; then
            echo "FATAL: test folder contains a mount or the mount list is invalid: $T" >&2
            exit 1
        fi
        private=$(mktemp -d "$base/layr-fixture-clean.XXXXXX") || exit 1
        trash="$private/trash"
        mkdir "$trash" || exit 1
        mv -- "$T" "$trash/test" || { echo "FATAL: cannot move test folder to $trash" >&2; exit 1; }
        "$LAYR" __empty-trash "$trash" || { echo "FATAL: cannot empty $trash" >&2; exit 1; }
        contents=$(ls -A "$trash") || { echo "FATAL: cannot inspect $trash" >&2; exit 1; }
        if [ -n "$contents" ] || [ -s "$private/trash.errors" ]; then
            echo "FATAL: test cleanup left files in $trash" >&2
            [ ! -f "$private/trash.errors" ] || cat "$private/trash.errors" >&2
            exit 1
        fi
        rmdir "$trash" || exit 1
        [ ! -f "$private/trash.lock" ] || unlink "$private/trash.lock" || exit 1
        [ ! -f "$private/trash.errors" ] || unlink "$private/trash.errors" || exit 1
        rmdir "$private" || exit 1
        return
    fi
    for mp in $(findmnt -rn -o TARGET | grep "^$T" | sort -r); do umount -l "$mp" 2>/dev/null; done
    while read -r device image; do
        case "$image" in "$T/"*) losetup -d "$device" 2>/dev/null || true ;; esac
    done < <(losetup --list --noheadings --output NAME,BACK-FILE)
    # A store that a test made below $T on the test file system holds read-only subvolumes (states):
    # rm cannot remove them. Delete them quietly, so the test's summary stays its last line.
    if ! rm -rf "$T" 2>/dev/null; then
        find "$T" -depth -type d -inum 256 2>/dev/null | while read -r d; do
            btrfs property set -ts "$d" ro false 2>/dev/null
            btrfs subvolume delete "$d" >/dev/null 2>&1
        done
        rm -rf "$T" 2>/dev/null
    fi
}
trap cleanup EXIT

new_fs "$LAYR_ROOT" 8G

ok() { # ok <description> <command...>: the command succeeds
    local d=$1; shift
    if ( "$@" ) >/dev/null 2>&1; then PASS=$((PASS + 1)); echo "PASS $d"; else FAIL=$((FAIL + 1)); echo "FAIL $d"; fi
}

no() { # no <description> <command...>: the command fails
    local d=$1; shift
    if ( "$@" ) >/dev/null 2>&1; then FAIL=$((FAIL + 1)); echo "FAIL $d (succeeded)"; else PASS=$((PASS + 1)); echo "PASS $d"; fi
}

eq() { # eq <description> <expected> <actual>
    if [ "$2" = "$3" ]; then PASS=$((PASS + 1)); echo "PASS $1"; else FAIL=$((FAIL + 1)); printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$1" "$2" "$3"; fi
}

has() { # has <description> <needle> <haystack>
    case "$3" in *"$2"*) PASS=$((PASS + 1)); echo "PASS $1" ;; *) FAIL=$((FAIL + 1)); printf 'FAIL %s\n  missing: %s\n  in: %s\n' "$1" "$2" "$3" ;; esac
}

# No garbage is left (docs/ownership.md): data goes in the operation that makes it dead, with no gc. At the end of
# each test the owners of every project of the test store equal the replay of its records with no leaked state
# (`layr gc`, a check), and no cache data is unreachable (`fsck --caches`). A test that makes garbage on purpose (a
# crash, a leak) sets LAYR_TEST_GARBAGE_OK=1.
garbage_check() {
    [ -n "${LAYR_TEST_GARBAGE_OK:-}" ] && return 0
    # A test that unsets LAYR_ROOT (set -u) has no store to check.
    [ -n "${LAYR_ROOT:-}" ] && [ -d "$LAYR_ROOT/projects" ] || return 0
    local d name out
    for d in "$LAYR_ROOT"/projects/*/; do
        name=$(basename "$d")
        case "$name" in *%*) continue ;; esac
        [ -f "$d/meta/db/layr.sqlite" ] || continue
        if ! out=$(cd / && unset LAYR_SOCKET && l --project "$name" gc 2>&1); then
            FAIL=$((FAIL+1)); echo "FAIL no garbage is left in project $name (layr gc)"; echo "$out" | sed 's/^/  /' | head -8
        fi
        if ! out=$(cd / && unset LAYR_SOCKET && l --project "$name" fsck --caches 2>&1); then
            FAIL=$((FAIL+1)); echo "FAIL no cache garbage is left in project $name (fsck --caches)"; echo "$out" | sed 's/^/  /' | head -8
        fi
    done
}

finish() {
    garbage_check
    echo "$(basename "$0"): $PASS passed, $FAIL failed"
    [ "$FAIL" -eq 0 ]
}

# A small project folder.
case_sensitive() { # true when the test folder tells README and readme apart
    touch "$T/.CaseProbe"
    if [ -e "$T/.caseprobe" ]; then rm -f "$T/.CaseProbe"; return 1; fi
    rm -f "$T/.CaseProbe"
}

sample() { # sample <dir>
    rm -rf "$1"; mkdir -p "$1/src" "$1/target"
    printf 'fn main() {\n    println!("hi");\n}\n' > "$1/src/main.rs"
    seq 1 20 | sed 's/^/line /' > "$1/notes.txt"
    printf 'target/\n*.log\n' > "$1/.gitignore"
    echo bin > "$1/target/out.bin"; echo x > "$1/debug.log"
}

# GNU and BSD (macOS) tools differ: sed -i needs a suffix argument on BSD, and stat has
# other format flags. These helpers run the same on both.
sed_inplace() { # sed_inplace <sed options> <script> <file>
    local file=${*: -1}
    sed -i.bak "${@:1:$#-1}" "$file" && rm -f "$file.bak"
}

stat_field() { # stat_field <inode|mode> <file>
    # Probe the flavor: CI puts GNU coreutils first in PATH even on macOS.
    if stat --version >/dev/null 2>&1; then
        case $1 in inode) stat -c %i "$2" ;; mode) stat -c %a "$2" ;; esac
    else
        case $1 in inode) stat -f %i "$2" ;; mode) stat -f %Lp "$2" ;; esac
    fi
}

# Run a peer command through a named remote with an explicit transport for tests.
peer_exchange() {
    local op=$1 project=$2; shift 2
    l --project "$project" remote add test-peer "ssh://test/$project" >/dev/null 2>&1 || true
    l --project "$project" "$op" test-peer --command "$@"
}
