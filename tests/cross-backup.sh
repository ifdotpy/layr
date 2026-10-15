#!/bin/bash
# Exchange backup artifacts between independent macOS and Linux CI runners.
set -euo pipefail
mode=${1:?create, extend, or check}
artifact=${2:?artifact folder}
output=${3:-}
. "$(dirname "$0")/lib.sh"
export LAYR_TEST_DIRECT_MAIN=1
if [ "$mode" = create ]; then
    mkdir -p "$T/src/empty" "$artifact"
    printf 'from mac\n' > "$T/src/file"
    ln -s file "$T/src/link"
    printf '#!/bin/sh\nexit 0\n' > "$T/src/run"
    chmod 755 "$T/src/run"
    l init cross --from "$T/src" -q
    cd "$LAYR_ROOT/projects/cross/lines/main"
    l rev-parse HEAD > "$artifact/mac-head"
    l backup -q "$artifact/backup"
else
    l backup restore -q "$artifact/backup"
    cd "$LAYR_ROOT/projects/cross/lines/main"
    head=$(cat "$artifact/mac-head")
    eq 'the Mac state keeps its identifier and content' 'from mac' "$(l show "$head:file")"
    eq 'the symbolic link survives' file "$(readlink link)"
    ok 'the executable mode survives' test -x run
    ok 'the empty directory survives' test -d empty
    ok 'the restored signatures pass fsck' l fsck
    if [ "$mode" = extend ]; then
        : "${output:?output folder}"
        mkdir -p "$output"
        cp "$artifact/mac-head" "$output/mac-head"
        printf 'from linux\n' > file
        l commit -qam linux
        l rev-parse HEAD > "$output/linux-head"
        l backup -q "$output/backup"
    elif [ "$mode" = check ]; then
        head=$(cat "$artifact/linux-head")
        eq 'the Linux state keeps its identifier and content' 'from linux' "$(l show "$head:file")"
    else
        echo 'Unknown mode' >&2
        exit 1
    fi
fi
finish
