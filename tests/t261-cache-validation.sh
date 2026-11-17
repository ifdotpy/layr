#!/bin/bash
# ci: macos macos-plain overlayfs
# Live unknown output is never labelled or deleted, even before the first save or during a move.
. "$(dirname "$0")/lib.sh"

# Keep the store and physical move destinations on the same test filesystem.
FS=$LAYR_ROOT
export LAYR_ROOT=$FS/store
mkdir -p "$LAYR_ROOT"

sample "$T/src"
echo 'target regenerable' > "$T/src/.layrattributes"
l init validation --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/validation/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
no "the new cache has no saved overlay yet" l overlay readers "$TG"
ok "creation established cold provenance before the build" test -f target/.layr-cache-location
cp target/.layr-cache-location "$T/known-location"
echo unsaved > target/output
cd "$T"
out=$(l --project validation line move main "$FS/known-unsaved-move" 2>&1); rc=$?
no "an unsaved cache with provenance participates in move validation" test "$rc" = 0
no "the known unsaved cache is not published at a foreign location" test -e "$FS/known-unsaved-move"
cd "$P/main"
rm target/.layr-cache-location
out=$(l save --overlays -q 2>&1); rc=$?
no "unsaved output without provenance cannot be saved" test "$rc" = 0
has "the refusal explains exclusion" 'cannot exclude active builds' "$out"
eq "unknown bytes remain intact" unsaved "$(cat target/output)"
no "unknown bytes acquire no marker" test -e target/.layr-cache-location
cd "$T"
out=$(l --project validation line move main "$FS/moved" 2>&1); rc=$?
no "moving an unsaved cache is refused before publication" test "$rc" = 0
no "the refused move publishes no destination" test -e "$FS/moved"
eq "the unsaved cache stays at its original location" unsaved "$(cat "$P/main/target/output")"

# Repeat with saved history and an active writer; history can never replace a missing live marker.
cp "$T/known-location" "$P/main/target/.layr-cache-location"
(cd "$P/main" && l save --overlays -q)
rm "$P/main/target/.layr-cache-location"
python3 - "$P/main/target/output" "$T/writer-ready" "$T/writer-go" <<'PY' &
import os, pathlib, sys, time
with open(sys.argv[1], 'a') as output:
    pathlib.Path(sys.argv[2]).touch()
    while not pathlib.Path(sys.argv[3]).exists(): time.sleep(.02)
    output.write('\nwriter survived\n'); output.flush(); os.fsync(output.fileno())
PY
writer=$!
for i in $(seq 1 100); do [ -e "$T/writer-ready" ] && break; sleep .02; done
ok "the active writer is ready before the reset" test -e "$T/writer-ready"
out=$(cd "$P/main" && l save --overlays -q 2>&1); rc=$?
no "an active cache reset is refused" test "$rc" = 0
ok "the writer's original file path remains present" test -f "$P/main/target/output"
no "saved history does not stamp a missing marker" test -e "$P/main/target/.layr-cache-location"
touch "$T/writer-go"
wait "$writer"
has "the writer keeps writing to the original path" 'writer survived' "$(cat "$P/main/target/output")"
printf '{broken\n' > "$P/main/target/.layr-cache-location"
out=$(cd "$P/main" && l save --overlays -q 2>&1); rc=$?
no "malformed provenance also fails closed" test "$rc" = 0
eq "the malformed marker is not replaced" '{broken' "$(cat "$P/main/target/.layr-cache-location")"

# A glob cannot discover an absent cache: the refusal gives an explicit initialization step.
sample "$T/wildcard-src"
echo 'build-* regenerable' > "$T/wildcard-src/.layrattributes"
echo 'build-*/' >> "$T/wildcard-src/.gitignore"
l init wildcard --from "$T/wildcard-src" -q >/dev/null
cd "$LAYR_ROOT/projects/wildcard/lines/main"
l protect main --remove >/dev/null 2>&1 || true
mkdir build-output
echo unknown > build-output/output
out=$(l save --overlays -q 2>&1); rc=$?
no "unproven wildcard output is refused" test "$rc" = 0
has "the refusal explains literal initialization" 'a wildcard alone cannot initialize an absent folder' "$out"
rm -rf build-output
l save --overlays -q >/dev/null
no "a glob alone cannot recreate the absent cache" test -e build-output
l config regenerable '["build-output"]' >/dev/null
ok "an exact configured path establishes cold provenance" test -f build-output/.layr-cache-location

# Versioned folders are a different type, even when an overlay and regenerable rule name them.
sample "$T/versioned-src"
echo 'target regenerable' > "$T/versioned-src/.layrattributes"
echo kept > "$T/versioned-src/target/kept.txt"
l init versioned --from "$T/versioned-src" --include-ignored -q >/dev/null
V=$LAYR_ROOT/projects/versioned/lines
cd "$V/main"
l protect main --remove >/dev/null 2>&1 || true
l overlay create tracked-folder --folder target --platform --quiet --history last -q
l line new -q copy main >/dev/null
eq "line creation preserves versioned files in a cache-named folder" kept "$(cat "$V/copy/target/kept.txt")"
echo unsaved-edit > target/kept.txt
cd "$T"
out=$(l --project versioned line move main "$FS/versioned-moved" 2>&1); rc=$?
eq "a versioned folder does not prevent a physical line move" 0 "$rc"
[ "$rc" = 0 ] || printf '%s\n' "$out"
eq "the physical move preserves unsaved versioned edits" unsaved-edit "$(cat "$FS/versioned-moved/target/kept.txt")"

# A save-back recipient's original head must not classify a later build's cache.
sample "$T/saveback-src"
l init saveback-context --from "$T/saveback-src" --include-ignored -q >/dev/null
cd "$LAYR_ROOT/projects/saveback-context/lines/main"
l protect main --remove >/dev/null 2>&1 || true
l line new -q "main.checks-$HERE_PLATFORM" main >/dev/null
old_head=$(l rev-parse "main.checks-$HERE_PLATFORM")
rm -rf target
l save -a -qm 'remove versioned build output' >/dev/null || { FAIL=$((FAIL+1)); echo 'FAIL save-back regression setup: removing versioned output'; finish; exit 1; }
echo 'target regenerable' > .layrattributes
l add .layrattributes >/dev/null
l save -qm 'declare build output disposable' >/dev/null || { FAIL=$((FAIL+1)); echo 'FAIL save-back regression setup: declaring disposable output'; finish; exit 1; }
l save --overlays -q >/dev/null
echo current-build > target/output
out=$(l save --overlays --save-back main --json); rc=$?
eq "save-back succeeds after a versioned folder becomes a cache" 0 "$rc"
has "save-back records the valid cache from the actual build context" '"path":"target"' "$out"
eq "save-back leaves its persistent recipient at the original head" "$old_head" "$(l rev-parse "main.checks-$HERE_PLATFORM")"
finish
