#!/bin/bash
# ci: macos macos-plain overlayfs
# Build-output compatibility includes the eventual workspace and target location. Legacy output is cold;
# a `cache-location-bound` folder is never shared, and failed relocation still rejects the whole cache.
. "$(dirname "$0")/lib.sh"

# Keep physical move probes on the store filesystem so they reach cache validation.
FS=$LAYR_ROOT
export LAYR_ROOT=$FS/store
mkdir -p "$LAYR_ROOT"

sample "$T/src"
echo 'target regenerable cache-location-bound' > "$T/src/.layrattributes"
printf '[package]\nname = "location"\nversion = "0.1.0"\n' > "$T/src/Cargo.toml"
l init locations --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/locations/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target
echo main > target/own
ok "save the first local build" l save --overlays -q

l line new -q other main >/dev/null
no "a new line starts cold" test -e "$P/other/target/own"
# The source line's own saved cache remains usable at exactly the same location.
cd "$T"
# Remove managed trees through the backend (OverlayFS mounts).
l __empty-trash "$P" || exit 1
ok "restore the same line at the same location" l --project locations adopt main --head
eq "same-location restoration stays warm" main "$(cat "$P/main/target/own")"
l --project locations adopt other --head >/dev/null || exit 1
cd "$P/other"
echo other > target/foreign
l save --overlays -q
cd "$P/main"
ok "a cache merge rejects the foreign location" l merge other --overlays-only -q
no "a merge never unions foreign output" test -e target/foreign
eq "the destination keeps its own compatible output" main "$(cat target/own)"

# A live reset cannot establish cross-platform exclusion: preserve output and refuse the move.
out=$(l line move other "$FS/moved-other" 2>&1); rc=$?
no "moving a live incompatible cache is refused" test "$rc" = 0
eq "the refused move preserves the source cache" other "$(cat "$P/other/target/foreign")"
no "the refused move publishes no destination" test -e "$FS/moved-other"

# A configured cache path may be a symlink; provenance must not write through it.
mkdir "$T/outside"
ln -s "$T/outside" linked-cache
l config regenerable '["linked-cache"]' >/dev/null
l save --overlays -q >/dev/null 2>&1 || true
no "cache provenance never writes through a folder symlink" test -e "$T/outside/.layr-cache-location"
rm linked-cache
l config regenerable '[]' >/dev/null

# Simulate pre-provenance saved data, and its live cache, without changing signed project history.
# Only the isolated fixture's disposable DB is changed. Next use must discard it, not relabel it.
python3 - "$LAYR_ROOT/projects/locations/meta/overlays/$TG/meta/db/layr.sqlite" <<'PY'
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    for sid, text in db.execute("SELECT id, rec FROM states").fetchall():
        rec = json.loads(text)
        rec.get("cache_key_parts", {}).pop("location", None)
        db.execute("UPDATE states SET rec=? WHERE id=?", (json.dumps(rec), sid))
PY
rm target/.layr-cache-location
out=$(l save --overlays -q 2>&1); rc=$?
no "a legacy live cache is refused without exclusion" test "$rc" = 0
eq "unknown legacy output is retained" main "$(cat target/own)"
no "legacy bytes were not stamped" test -e target/.layr-cache-location
# Restoring into a newly created copy can safely start cold; the original unknown output stays intact.
cd "$T"
cp -a "$P/main" "$T/legacy-aside" || exit 1
l __empty-trash "$P" || exit 1
l --project locations adopt main --head >/dev/null || exit 1
no "legacy saved output is not restored" test -e "$P/main/target/own"
eq "restoration preserves the original unknown output" main "$(cat "$T/legacy-aside/target/own")"

# The project path-list opt-in is configured before the first cache is established.
sample "$T/portable-src"
printf '[package]\nname = "location"\nversion = "0.1.0"\n' > "$T/portable-src/Cargo.toml"
l init portable-project --from "$T/portable-src" -q >/dev/null
P=$LAYR_ROOT/projects/portable-project/lines
cd "$P/main"
l protect main --remove >/dev/null 2>&1 || true
l config cache.portable '["target"]' >/dev/null
l config regenerable '["target"]' >/dev/null
echo portable > target/shared
l save --overlays -q
l line new -q portable main >/dev/null
eq "the project path-list opt-in shares output" portable "$(cat "$P/portable/target/shared")"

# A real Cargo relocation error: a binary dep-info entry is a directory. Moving a unit whose
# workspace manifest is unchanged reaches the read of that entry and fails even when run as root.
l overlay set "$TG" --prune none >/dev/null
mkdir -p target/debug/deps target/debug/.fingerprint/unit-aaa/dep-broken
printf '%s/target/debug/deps/unit-aaa.d: %s/Cargo.toml\n' "$PWD" "$PWD" > target/debug/deps/unit-aaa.d
l save --overlays -q
l overlay set "$TG" --prune cargo >/dev/null
out=$(l line new broken main 2>&1)
no "failed relocation exposes no shared output" test -e "$P/broken/target/shared"
has "failed relocation reports the cold fallback" 'starting cold' "$out"

# A second project records the opt-in in its initial, versioned attributes.
sample "$T/attribute-src"
echo 'target regenerable cache-portable' > "$T/attribute-src/.layrattributes"
l init attributes --from "$T/attribute-src" -q >/dev/null
cd "$LAYR_ROOT/projects/attributes/lines/main"
l protect main --remove >/dev/null 2>&1 || true
mkdir -p target
echo attribute > target/shared
l save --overlays -q
l line new -q attribute main >/dev/null
eq "the attribute opt-in shares output" attribute "$(cat "$LAYR_ROOT/projects/attributes/lines/attribute/target/shared")"
finish
