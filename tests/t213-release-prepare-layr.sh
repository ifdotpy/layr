#!/bin/bash
# ci: macos overlayfs
# Prepare a release from Layr states without a Git checkout or Git executable.
. "$(dirname "$0")/lib.sh"
[ "$(id -u)" != 0 ] || { echo "$(basename "$0") (not as root): 0 passed, 0 failed"; exit 0; }

sample "$T/src"
printf '{".":"0.1.0"}\n' > "$T/src/.release-please-manifest.json"
printf '{"changelog-sections":[{"type":"feat","section":"Features"},{"type":"fix","section":"Bug Fixes"}]}\n' > "$T/src/release-please-config.json"
printf '[workspace.package]\nversion = "0.1.0"\n' > "$T/src/Cargo.toml"
printf '[[package]]\nname = "layr"\nversion = "0.1.0"\n\n[[package]]\nname = "layr-api"\nversion = "0.1.0"\n' > "$T/src/Cargo.lock"
printf '# Changelog\n\n' > "$T/src/CHANGELOG.md"
l init p --from "$T/src" -q >/dev/null
cd "$LAYR_ROOT/projects/p/lines/main" || exit 1
l ref set release/v0.1.0 main >/dev/null
eq "JSON reports that no release is needed" none "$(l release prepare --dry-run --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
echo feature >> notes.txt
l add notes.txt
l save -qm 'feat: add a feature'
mkdir "$T/bin"
printf '#!/bin/sh\necho called > "%s"\nexit 99\n' "$T/git-called" > "$T/bin/git"
chmod +x "$T/bin/git"
export PATH="$T/bin:$PATH"
out=$(l release prepare --dry-run --date 2026-11-07 2>&1); rc=$?
eq "dry run succeeds from Layr history" 0 "$rc"
has "the release has the new feature" 'add a feature' "$out"
has "the feature bumps the version" '0.2.0' "$out"
no "the command never runs Git" test -e "$T/git-called"
eq "JSON reports the proposed version" 0.2.0 "$(l release prepare --dry-run --json --date 2026-11-07 | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["status"] == "ready" and d["base"] and d["head"]; print(d["version"])')"
cp Cargo.lock "$T/good.lock"
printf 'invalid lock\n' > Cargo.lock
no "invalid input refuses a release" l release prepare --date 2026-11-07
eq "invalid input does not change the changelog" '# Changelog' "$(cat CHANGELOG.md)"
has "invalid input does not change Cargo.toml" '0.1.0' "$(cat Cargo.toml)"
cp "$T/good.lock" Cargo.lock
ok "the release files are updated" l release prepare --date 2026-11-07
has "the manifest has the new version" '0.2.0' "$(cat .release-please-manifest.json)"
has "the changelog has the feature" 'add a feature' "$(cat CHANGELOG.md)"
no "the command never runs Git during write" test -e "$T/git-called"
l line new -q other main~1
cd ../other || exit 1
echo unrelated >> notes.txt
l add notes.txt
l save -qm 'fix: unrelated'
cd ../main || exit 1
l ref set release/v0.1.0 other >/dev/null
printf '{".":"0.1.0"}\n' > .release-please-manifest.json
no "an unrelated previous release is refused" l release prepare --dry-run
finish
