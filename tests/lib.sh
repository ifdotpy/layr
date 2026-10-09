# Test helpers. Each test file gets a fresh btrfs file system on a loop file.
# Needs root, btrfs-progs, git, zstd. LAYR points at the binary under test.

set -u
LAYR=${LAYR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/layr}
PASS=0
FAIL=0
T=$(mktemp -d /var/tmp/layr-test.XXXXXX)
export LAYR_ROOT=$T/store
export GIT_AUTHOR_NAME=Tester GIT_AUTHOR_EMAIL=tester@example.com GIT_COMMITTER_NAME=Tester GIT_COMMITTER_EMAIL=tester@example.com
export LAYR_AUTHOR_NAME=Tester LAYR_AUTHOR_EMAIL=tester@example.com
unset LAYR_PROJECT LAYR_LINE LAYR_SOCKET

l() { "$LAYR" "$@"; }

new_fs() { # new_fs <mountpoint> <size>
    local mp=$1 size=${2:-8G}
    truncate -s "$size" "$mp.img"
    mkfs.btrfs -q -f "$mp.img"
    mkdir -p "$mp"
    mount -o loop,user_subvol_rm_allowed "$mp.img" "$mp"
}

cleanup() {
    cd /
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
