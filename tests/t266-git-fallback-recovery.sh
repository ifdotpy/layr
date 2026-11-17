#!/bin/bash
# ci: macos
# Recover an unborn fallback without deleting unrelated or subsequently changed branch refs.
. "$(dirname "$0")/lib.sh"

git init -q -b main "$T/up"
echo base > "$T/up/file"
git -C "$T/up" add .
git -C "$T/up" commit -qm base
l git clone "$T/up" recovery -q >/dev/null
# Portable parent reads let this regression complete a save without privileged Btrfs metadata sends.
# Park the clean working folder so migration only reads the imported Git state.
# Stay inside the store filesystem: moving across its mount would copy the subvolume as a plain directory.
# Overlayfs mounts the line folder, so this move cannot run there; the recovery itself is filesystem independent.
working=$LAYR_ROOT/projects/recovery/lines/main
mv "$working" "$LAYR_ROOT/recovery-working" || exit 1
(cd "$T" && l --project recovery migrate --delete-old) >/dev/null; rc=$?
mv "$LAYR_ROOT/recovery-working" "$working" || exit 1
[ "$rc" = 0 ] || exit 1
cd "$working"
# Keep the new save native to avoid an incremental object write from the retained Git build cache.
l config objects.enabled false
head=$(python3 - "$LAYR_ROOT/projects/recovery/meta/db/layr.sqlite" <<'PY'
import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("SELECT head FROM lines WHERE name='main'").fetchone()[0])
PY
)
# Recreate the exact HEAD/marker pair from heads-only materialization without needing a remote archive.
chmod -R u+w .git
git update-ref -d refs/heads/main
git symbolic-ref HEAD refs/heads/main
printf '%s\nref: refs/heads/main\n' "$head" > .git/layr-fallback
no "the fallback is unborn" git rev-parse --verify HEAD
# A separate main must survive recovery while HEAD names another branch (reflogs are disabled).
git symbolic-ref HEAD refs/heads/feature
git commit --allow-empty -qm feature-base
git checkout -q --orphan main
echo important-history > file
git add file
git commit -qm important-main
important=$(git rev-parse refs/heads/main)
git checkout -q feature
echo current-feature > file
git add file
git commit -qm current-feature
feature=$(git rev-parse refs/heads/feature)
eq "main is the important commit's only reference" refs/heads/main "$(git for-each-ref --contains "$important" --format='%(refname)')"
no "no reflog preserves the important commit" test -d .git/logs
out=$(l save -a --allow-empty -qm blocked 2>&1); rc=$?
eq "save refuses HEAD on another branch" 128 "$rc"
recovery=$(printf '%s\n' "$out" | sed -n '/^  printf /p')
ok "another-branch diagnostic includes a recovery command" test -n "$recovery"
eq "another-branch recovery deletes no refs" "$recovery" "${recovery#*update-ref}"
ok "another-branch recovery runs" eval "$recovery"
eq "recovery preserves the separate main" "$important" "$(git rev-parse --verify refs/heads/main)"
eq "important history stays reachable" refs/heads/main "$(git for-each-ref --contains "$important" --format='%(refname)')"
eq "recovery preserves the feature ref" "$feature" "$(git rev-parse --verify refs/heads/feature)"

# A symbolic fallback ref may itself have been redirected to a branch the user cares about.
chmod -R u+w .git
git symbolic-ref refs/heads/main refs/heads/feature
out=$(l save -a --allow-empty -qm blocked 2>&1); rc=$?
eq "save refuses a redirected fallback ref" 128 "$rc"
recovery=$(printf '%s\n' "$out" | sed -n '/^  printf /p')
ok "redirected-ref diagnostic includes a recovery command" test -n "$recovery"
eq "redirected-ref recovery deletes no refs" "$recovery" "${recovery#*update-ref}"
ok "redirected-ref recovery runs" eval "$recovery"
eq "recovery preserves the redirected ref" refs/heads/feature "$(git symbolic-ref refs/heads/main)"
eq "recovery preserves the redirected target" "$feature" "$(git rev-parse --verify refs/heads/feature)"

# Even a quoted branch name must remain one shell argument in the advertised command.
quoted_ref="refs/heads/fallback'branch"
git symbolic-ref HEAD "$quoted_ref"
printf '%s\nref: %s\n' "$head" "$quoted_ref" > .git/layr-fallback
git commit --allow-empty -qm quoted-outside
out=$(l save -a --allow-empty -qm blocked 2>&1); rc=$?
eq "save refuses an external commit on a quoted fallback" 128 "$rc"
recovery=$(printf '%s\n' "$out" | sed -n '/^  printf /p')
ok "quoted-ref diagnostic includes a recovery command" test -n "$recovery"
ok "quoted-ref recovery runs" eval "$recovery"
eq "recovery preserves the quoted symbolic HEAD" "ref: $quoted_ref" "$(cat .git/HEAD)"
no "quoted-ref recovery makes HEAD unborn" git rev-parse --verify HEAD
eq "quoted-ref recovery preserves the other branch" "$feature" "$(git rev-parse --verify refs/heads/feature)"
git symbolic-ref HEAD refs/heads/main
printf '%s\nref: refs/heads/main\n' "$head" > .git/layr-fallback

# Return to the original first-commit case on the unborn fallback.
chmod -R u+w .git
git update-ref --no-deref -d refs/heads/main
echo base > file
git add file
git commit --allow-empty -qm outside
eq "the first external commit leaves HEAD symbolic" "ref: refs/heads/main" "$(cat .git/HEAD)"
out=$(l save -a --allow-empty -qm blocked 2>&1); rc=$?
eq "save refuses the external commit" 128 "$rc"
has "save explains the Git change" "A git commit, merge, reset or checkout ran in this line" "$out"
recovery=$(printf '%s\n' "$out" | sed -n '/^  printf /p')
ok "the diagnostic includes a recovery command" test -n "$recovery"
# The emitted deletion must fail atomically if the branch changed after the diagnostic.
observed=$(git rev-parse HEAD)
has "recovery compares against the observed commit" "git update-ref --no-deref -d 'refs/heads/main' '$observed'" "$recovery"
git commit --allow-empty -qm newer
newer=$(git rev-parse HEAD)
no "a stale recovery refuses to delete the changed ref" eval "$recovery"
eq "stale recovery preserves the newer commit" "$newer" "$(git rev-parse --verify refs/heads/main)"
git update-ref refs/heads/main "$observed" "$newer"
ok "the printed recovery command runs" eval "$recovery"
eq "recovery restores the symbolic HEAD" "ref: refs/heads/main" "$(cat .git/HEAD)"
no "recovery makes the fallback unborn again" git rev-parse --verify HEAD
# An empty named save still checks HEAD and publishes a new head.
out=$(l save -a --allow-empty -qm recovered 2>&1); rc=$?
eq "save succeeds after the printed recovery" 0 "$rc"
[ "$rc" = 0 ] || printf '%s\n' "$out"
eq "recovery preserves the working file" base "$(cat file)"
no "save clears the fallback marker" test -e .git/layr-fallback

# The fixture deliberately opens the managed Git folder for the external commit; open it for cleanup too.
chmod -R u+w .git
finish
