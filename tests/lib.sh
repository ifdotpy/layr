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
export LAYR_ROOT=$T/store
export GIT_AUTHOR_NAME=Tester GIT_AUTHOR_EMAIL=tester@example.com GIT_COMMITTER_NAME=Tester GIT_COMMITTER_EMAIL=tester@example.com
export LAYR_AUTHOR_NAME=Tester LAYR_AUTHOR_EMAIL=tester@example.com
unset LAYR_PROJECT LAYR_LINE LAYR_SOCKET

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
        chflags -R nouchg "$T" 2>/dev/null || true
        chmod -R u+w "$T" 2>/dev/null || true
        rm -rf "$T"
        return
    fi
    for mp in $(findmnt -rn -o TARGET | grep "^$T" | sort -r); do umount -l "$mp" 2>/dev/null; done
    while read -r device image; do
        case "$image" in "$T/"*) losetup -d "$device" 2>/dev/null || true ;; esac
    done < <(losetup --list --noheadings --output NAME,BACK-FILE)
    rm -rf "$T"
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

finish() {
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
