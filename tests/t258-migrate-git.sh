#!/bin/bash
# ci: macos overlayfs
# layr migrate of a project made from Git (docs/objects.md, section 13): a state that `build_tree`
# builds from Git is verified against its Git commit, not by a checkout (L10). History: plain and
# executable files, a symbolic link, nested folders, a submodule, an edit, a delete, a rename and a
# mode change. Every imported state's v2 tree then checks out equal to `git archive` of its commit
# (names, kinds, modes, bytes, link targets; the submodule is an empty folder), compared by a second
# walk. LAYR_MIGRATE_VERIFY_GIT=0 checks every state out instead, with the same result.
. "$(dirname "$0")/lib.sh"
B=$LAYR_ROOT
export LAYR_ROOT=$B/store
mkdir -p "$LAYR_ROOT"
umask 022

history() { # history <dir>: a Git checkout with seven commits
    git init -q "$1" && cd "$1" || exit 1
    git config user.email t@example.invalid && git config user.name t
    mkdir -p src/deep/er && echo one > a.txt && echo deep > src/deep/er/x.txt
    printf '#!/bin/sh\necho hi\n' > run.sh && chmod 755 run.sh
    git add . && git commit -qm "files" && git branch -M main
    ln -s a.txt link && git add link && git commit -qm "link"
    git update-index --add --cacheinfo 160000,0123456789abcdef0123456789abcdef01234567,sub && git commit -qm "submodule"
    echo two >> a.txt && git commit -qam "edit"
    git rm -q src/deep/er/x.txt && mkdir -p src && echo y > src/y.txt && git add src/y.txt && git commit -qm "delete and add"
    git mv a.txt b.txt && git commit -qm "rename"
    chmod 755 src/y.txt && git commit -qam "mode"
    cd - >/dev/null || exit 1
}
# Compare a checkout with `git archive` of a commit: names, kinds, permission bits, bytes, link targets.
# The submodule is an empty folder in layr and absent from the archive.
same_as_git() { # same_as_git <checkout> <git dir> <commit>
    local tmp; tmp=$(mktemp -d "$B/arch.XXXXXX")
    # git archive writes its modes through tar.umask (0002 by default), and tar as root keeps them; 0022 gives git's modes.
    git -C "$2" -c tar.umask=0022 archive "$3" | tar -x -C "$tmp"
    python3 -I - "$1" "$tmp" <<'EOF'
import os, stat, sys
def walk(root):
    out = {}
    for d, dirs, files in os.walk(root):
        for name in dirs + files:
            p = os.path.join(d, name); rel = os.path.relpath(p, root)
            if rel == ".git" or rel.startswith(".git/"):
                continue
            m = os.lstat(p)
            kind = "l" if stat.S_ISLNK(m.st_mode) else "d" if stat.S_ISDIR(m.st_mode) else "f"
            if kind == "d" and rel == "sub" and not os.listdir(p):
                continue
            body = os.readlink(p) if kind == "l" else open(p, "rb").read() if kind == "f" else None
            out[rel] = (kind, 0 if kind == "l" else m.st_mode & 0o777, body)
    return out
a, b = walk(sys.argv[1]), walk(sys.argv[2])
for rel in sorted(set(a) | set(b)):
    if a.get(rel) != b.get(rel):
        sys.exit(print("differs:", rel, a.get(rel, "absent")[:2], b.get(rel, "absent")[:2]) or 1)
print(len(a))
EOF
    local rc=$?
    rm -rf "$tmp"
    return $rc
}
DB() { # DB <project> <sql>
    python3 -I -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); [print("|".join(str(v) for v in r)) for r in c.execute(sys.argv[2])]' \
        "$LAYR_ROOT/projects/$1/meta/db/layr.sqlite" "$2"
}
check_all() { # check_all <project> <line folder>: every imported state checks out equal to its commit
    local n=0 bad=0
    while IFS='|' read -r state commit; do
        local out; out=$(mktemp -d "$B/co.XXXXXX"); rmdir "$out"
        if (cd "$2" && l objects checkout "$state" "$out" >/dev/null 2>&1) &&
            same_as_git "$out" "$B/ref" "$commit" >/dev/null; then n=$((n + 1)); else bad=$((bad + 1)); fi
        chmod -R u+w "$out" 2>/dev/null; rm -rf "$out"
    done < <(DB "$1" "SELECT m.state, m.gcommit FROM git_map m JOIN states s ON s.id = m.state WHERE s.kind = 'import'")
    echo "$n $bad"
}

history "$B/git"
# `init --from-git` makes the checkout the line in place: a clone for the second project, one to compare with.
git clone -q "$B/git" "$B/git2" && git clone -q --bare "$B/git" "$B/ref"
# --snapshots: without it init migrates the project itself, and `layr migrate` below has nothing to do.
ok "a project is made from the Git history" "$LAYR" init gp --from-git "$B/git" --snapshots
cd "$B/git" || exit 1
imports=$(DB gp "SELECT count(*) FROM states WHERE kind = 'import'")
ok "the history imports as states" test "$imports" -ge 7
out=$(l migrate 2>&1); rc=$?
eq "the migration passes" 0 "$rc"
git_verified=$(printf '%s\n' "$out" | grep -c 'equal to its Git commit')
ok "states built from Git are verified against their commits" test "$git_verified" -ge 1
eq "every imported state checks out equal to its commit" "$imports 0" "$(check_all gp "$B/git")"
ok "fsck passes after the migration" l fsck
eq "a file reads back" "y" "$(l show main:src/y.txt)"

# The same history with the full comparison for every state.
cd "$B" || exit 1
ok "a second project is made from the same history" "$LAYR" init gq --from-git "$B/git2" --snapshots
cd "$B/git2" || exit 1
out=$(LAYR_MIGRATE_VERIFY_GIT=0 l migrate 2>&1); rc=$?
eq "the migration with checkouts passes" 0 "$rc"
eq "no state is verified against Git when it is switched off" 0 "$(printf '%s\n' "$out" | grep -c 'equal to its Git commit')"
eq "both migrations give the same trees" \
    "$(DB gp "SELECT m.gcommit, json_extract(s.rec,'\$.tree') FROM git_map m JOIN states s ON s.id = m.state ORDER BY m.gcommit")" \
    "$(DB gq "SELECT m.gcommit, json_extract(s.rec,'\$.tree') FROM git_map m JOIN states s ON s.id = m.state ORDER BY m.gcommit")"

finish
