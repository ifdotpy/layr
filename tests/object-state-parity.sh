#!/bin/bash
# Object states remain usable without persistent native state trees.
# Run with a private LAYR_ROOT and matching daemon on Linux.
set -euo pipefail
[ "$(id -u)" != 0 ] || { echo 'Run as a non-root user.' >&2; exit 1; }
: "${LAYR:?}" "${LAYR_ROOT:?}"
export LAYR_AUTHOR_NAME=Objects LAYR_AUTHOR_EMAIL=objects@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null
unset LAYR_PROJECT LAYR_LINE
no_native_sources() {
    # The service owns states/ on Linux. Ask it to inspect the real UUID paths.
    "$LAYR" migrate --dry-run --delete-old > "$work/native-report"
    cat "$work/native-report"
    grep -Fq 'snapshots of source states here: 0 B in 0 state(s)' "$work/native-report"
    grep -Fq 'in 0 snapshot(s)' "$work/native-report"
}
work=$(mktemp -d "${TMPDIR:-/tmp}/layr-objects.XXXXXX")
project="objects-$(date +%s)-$$"
mkdir -p "$work/source/sub"
printf 'original\n' > "$work/source/file"
printf 'nested\n' > "$work/source/sub/file"
printf 'ignored/\n' > "$work/source/.gitignore"
ln "$work/source/file" "$work/source/hardlink"
ln -s file "$work/source/link"
chmod 755 "$work/source/sub/file"
"$LAYR" init "$project" --from "$work/source" -q
base="$LAYR_ROOT/projects/$project"
cd "$base/lines/main"
"$LAYR" protect main --remove
"$LAYR" migrate --delete-old
initial=$("$LAYR" rev-parse head)
no_native_sources
[ "$("$LAYR" show head:file)" = original ]
[ -z "$("$LAYR" status --porcelain)" ]
"$LAYR" line new branch
cd "$base/lines/branch"
[ "$(cat file)" = original ]
[ "$(readlink link)" = file ]
python3 - <<'PY'
import os
assert os.stat('file').st_ino == os.stat('hardlink').st_ino
assert os.stat('sub/file').st_mode & 0o777 == 0o755
PY
printf 'changed!\n' > file
mkdir -p added ignored
printf 'added\n' > added/file
printf 'cache\n' > ignored/file
"$LAYR" add file hardlink added
"$LAYR" save -m 'Change object state'
saved=$("$LAYR" rev-parse head)
no_native_sources
[ "$("$LAYR" show head:file)" = 'changed!' ]
[ "$("$LAYR" show head~1:file)" = original ]
"$LAYR" diff head~1 head > "$work/diff"
grep -q '+changed!' "$work/diff"
grep -q '+added' "$work/diff"
[ -z "$("$LAYR" status --porcelain)" ]
printf 'dirty\n' > file
"$LAYR" restore file
[ "$(cat file)" = 'changed!' ]
"$LAYR" line new copy
cd "$base/lines/copy"
[ "$(cat added/file)" = added ]
[ "$(cat file)" = 'changed!' ]
[ ! -e ignored/file ]
"$LAYR" log --oneline > "$work/log"
grep -q 'Change object state' "$work/log"
no_native_sources
"$LAYR" export head "$work/export"
[ "$(cat "$work/export/file")" = 'changed!' ]
"$LAYR" reset --hard "$initial"
[ "$(cat file)" = original ]
"$LAYR" undo
[ "$(cat file)" = 'changed!' ]
printf 'parked\n' > added/file
"$LAYR" park -q
[ "$(cat added/file)" = added ]
"$LAYR" park pop
[ "$(cat added/file)" = parked ]
"$LAYR" restore added/file
cd "$base/lines/main"
printf 'main only\n' > main-file
"$LAYR" add main-file
"$LAYR" save -m 'Independent main change'
[ "$("$LAYR" show head:.gitignore)" = 'ignored/' ]
[ "$("$LAYR" show head:sub/file)" = nested ]
[ "$("$LAYR" show head:link)" = file ]
"$LAYR" merge branch -m 'Merge object histories'
[ "$(cat file)" = 'changed!' ]
[ "$(cat main-file)" = 'main only' ]
[ "$(cat added/file)" = added ]
"$LAYR" fsck
no_native_sources
"$LAYR" config regenerable '["ignored"]'
mkdir -p ignored
printf 'cache content\n' > ignored/file
"$LAYR" save --overlays
[ "$(cat ignored/file)" = 'cache content' ]
"$LAYR" line new with-cache
cd "$base/lines/with-cache"
[ "$(cat file)" = 'changed!' ]
"$LAYR" fsck --overlays
printf 'PASS object state lifecycle (%s)\n' "$(uname -s)"
