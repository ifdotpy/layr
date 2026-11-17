#!/bin/bash
# ci: macos macos-plain overlayfs
# Only a successful repair establishes provenance for output built at this location.
. "$(dirname "$0")/lib.sh"

owner_args=()
if [ "$(id -u)" = 0 ]; then
    # Hooks skip root-owned lines. Use an isolated owner, as the other adapter integration tests do.
    OWN=layr-npm-repair
    id "$OWN" >/dev/null 2>&1 || useradd -m "$OWN"
    chmod 755 "$T" "$LAYR_ROOT"
    cp "$LAYR" "$T/layr" && chmod 755 "$T/layr" && LAYR=$T/layr
    owner_args=(--owner "$OWN")
fi
mkdir "$T/src" "$T/npm-cache"
if [ "$(id -u)" = 0 ]; then chown "$OWN" "$T/npm-cache"; fi
cat > "$T/src/package.json" <<'JSON'
{"name":"repair-provenance","version":"1.0.0","private":true,"scripts":{"postinstall":"node -e \"require('fs').mkdirSync('node_modules',{recursive:true});require('fs').writeFileSync('.repair-ran','repaired');if(require('fs').existsSync('.repair-fail'))process.exit(1);if(require('fs').existsSync('.repair-foreign'))require('fs').copyFileSync('.repair-foreign','node_modules/.layr-cache-location')\""}}
JSON
cat > "$T/src/package-lock.json" <<'JSON'
{"name":"repair-provenance","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"repair-provenance","version":"1.0.0","hasInstallScript":true}}}
JSON
printf 'cache=%s/npm-cache\noffline=true\nregistry=http://127.0.0.1:9/\naudit=false\nfund=false\n' "$T" > "$T/src/.npmrc"
printf 'node_modules/\n.repair-ran\n.repair-fail\n.repair-foreign\n' > "$T/src/.gitignore"
echo 'node_modules regenerable' > "$T/src/.layrattributes"
l init npm-repair --from "$T/src" ${owner_args[@]+"${owner_args[@]}"} -q >/dev/null || exit 1
cd "$LAYR_ROOT/projects/npm-repair/lines/main"
l protect main --remove >/dev/null 2>&1 || true
echo disposable > node_modules/output
ok "save the valid npm cache" l save --overlays --folder node_modules -q
cp node_modules/.layr-cache-location "$T/provenance"
l line list --json > "$T/lines.json"
line_id=$(python3 -c 'import json,sys; print(next(l["id"] for l in json.load(open(sys.argv[1])) if l["name"] == "main"))' "$T/lines.json")
damage="$LAYR_ROOT/projects/npm-repair/meta/line-locks/$line_id.damaged"
printf '["node_modules"]\n' > "$damage"
touch .repair-fail
out=$(l save --overlays --folder node_modules -q 2>&1)
has "a failed repair stays marked damaged" 'stays marked damaged' "$out"
eq "the failed install ran its script" repaired "$(cat .repair-ran)"
no "failed repair leaves output unproven" test -e node_modules/.layr-cache-location
has "failed repair gives explicit cleanup instructions" 'remove the cache folder' "$out"
ok "failed repair retains the damage flag" test -e "$damage"
rm .repair-fail .repair-ran
out=$(l save --overlays --folder node_modules -q 2>&1); rc=$?
no "a later save refuses unproven failed output" test "$rc" = 0
has "the later refusal explains recovery" 'Stop builds and remove the cache folder' "$out"
no "the installer does not run on failed unknown output" test -e .repair-ran
# The documented recovery discards output explicitly, then establishes cold provenance.
rm -rf node_modules
ok "repair initializes cold output after explicit cleanup" l overlay repair "$(overlay node_modules)"
out=$(l save --overlays --folder node_modules -q 2>&1); rc=$?
eq "a save repairs and saves the damaged npm cache" 0 "$rc"
[ "$rc" = 0 ] || printf '%s\n' "$out"
has "the real npm repair ran" 'repaired with npm' "$out"
has "the repaired cache was pruned" 'pruned with npm' "$out"
eq "npm ci ran its install script" repaired "$(cat .repair-ran)"
ok "successful repair stamps its actual build location" cmp "$T/provenance" node_modules/.layr-cache-location
no "successful repair clears the damage" test -e "$damage"
ok "the repaired npm cache can be saved again" l save --overlays --folder node_modules -q

# A successful installer must not overwrite a conflicting marker it finds in its result.
python3 - "$T/provenance" .repair-foreign <<'PYFIXTURE'
import json, sys
p = json.load(open(sys.argv[1]))
p["location"] = {"workspace": "/foreign", "target": "/foreign/node_modules"}
with open(sys.argv[2], "w") as f:
    json.dump(p, f)
PYFIXTURE
printf '["node_modules"]\n' > "$damage"
out=$(l save --overlays --folder node_modules -q 2>&1); rc=$?
no "a foreign marker from the installer is refused" test "$rc" = 0
has "the refusal identifies the conflict" 'conflicting location provenance' "$out"
ok "the conflicting marker is not overwritten" cmp .repair-foreign node_modules/.layr-cache-location
ok "the conflicting result remains damaged" test -e "$damage"
rm .repair-foreign .repair-ran
rm -rf node_modules
ok "explicit cleanup recovers from a conflicting repair" l overlay repair "$(overlay node_modules)"
ok "a new successful repair can save after cleanup" l save --overlays --folder node_modules -q

rm node_modules/.layr-cache-location .repair-ran
echo unknown > node_modules/output
printf '["node_modules"]\n' > "$damage"
no "damage repair never adopts unknown output" l save --overlays --folder node_modules -q
no "unknown output acquires no provenance" test -e node_modules/.layr-cache-location
no "the installer never runs on unknown output" test -e .repair-ran
eq "unknown output remains intact" unknown "$(cat node_modules/output)"
finish
