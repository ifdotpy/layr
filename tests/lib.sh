# Test helpers. Each test file gets a fresh btrfs file system on a loop file.
# Needs root, btrfs-progs, git, zstd. LAYR points at the binary under test.

set -u
LAYR=${LAYR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/layr}
PASS=0
FAIL=0
if [ "$(uname -s)" = Darwin ]; then
    : "${LAYR_APFS_TEST_ROOT:?Set LAYR_APFS_TEST_ROOT to a case-sensitive APFS volume}"
    T=$(mktemp -d "$LAYR_APFS_TEST_ROOT/layr-test.XXXXXX")
else
    T=$(mktemp -d /var/tmp/layr-test.XXXXXX)
fi
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
        case "${1:-}" in init) project=$2 ;; clone) project=$3 ;; esac
        if [ -n "$project" ]; then
            (cd "$LAYR_ROOT/projects/$project/lines/main" && "$LAYR" protect main --remove) || return $?
        fi
    fi
    return "$rc"
}

new_fs() { # new_fs <mountpoint> <size>
    local mp=$1 size=${2:-8G}
    if [ "$(uname -s)" = Darwin ]; then
        mkdir -p "$mp"
        return
    fi
    truncate -s "$size" "$mp.img"
    mkfs.btrfs -q -f "$mp.img"
    mkdir -p "$mp"
    mount -o loop,user_subvol_rm_allowed "$mp.img" "$mp"
}

cleanup() {
    cd /
    if [ "$(uname -s)" = Darwin ]; then
        chflags -R nouchg "$T" 2>/dev/null || true
        rm -rf "$T"
        return
    fi
    for mp in $(findmnt -rn -o TARGET | grep "^$T" | sort -r); do umount -l "$mp" 2>/dev/null; done
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
sample() { # sample <dir>
    rm -rf "$1"; mkdir -p "$1/src" "$1/target"
    printf 'fn main() {\n    println!("hi");\n}\n' > "$1/src/main.rs"
    seq 1 20 | sed 's/^/line /' > "$1/notes.txt"
    printf 'target/\n*.log\n' > "$1/.gitignore"
    echo bin > "$1/target/out.bin"; echo x > "$1/debug.log"
}
