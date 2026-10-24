#!/bin/bash
# Output formats: the same edits in a git checkout and in a layr line give the same output.
. "$(dirname "$0")/lib.sh"

R=$T/r.git; G=$T/g
git init -q --bare -b main $R; git clone -q $R $G 2>/dev/null; cd $G
seq 1 30 | sed 's/^/line /' > a.txt; printf 'no newline' > nonl.txt; printf 'x\0y' > bin.dat
mkdir -p d; echo keep > d/keep.txt; echo gone > gone.txt; echo mv > move-me.txt; printf '#!/bin/sh\n' > run.sh
printf 'fn main() {\n    let x = 1;\n    println!("{}", x);\n}\n' > code.rs
git add -A; git commit -qm base; git push -q origin main
l git clone $R c -q >/dev/null
Lw=$LAYR_ROOT/projects/c/lines/main

edit() { # the same edits in both trees
    cd $1
    sed -i 's/^line 3$/LINE THREE/; s/^line 20$/line twenty/; $ a line 31' a.txt
    printf 'no newline changed' > nonl.txt; printf 'x\0z' > bin.dat; rm gone.txt; chmod +x run.sh
    sed -i 's/let x = 1;/let x = 2;/' code.rs; echo new > new.txt
}
edit $G; edit $Lw

cmp_out() { # cmp_out <description> <args...>: git and layr print the same
    local d=$1; shift
    local g w arg
    local -a layr_args=()
    for arg in "$@"; do
        case "$arg" in
            HEAD|HEAD[~^:@]*) layr_args+=("head${arg#HEAD}") ;;
            *) layr_args+=("$arg") ;;
        esac
    done
    g=$(cd $G && git -c core.abbrev=7 "$@" 2>&1); w=$(cd $Lw && l "${layr_args[@]}" 2>&1)
    if [ "$g" = "$w" ]; then PASS=$((PASS + 1)); echo "PASS $d"; else FAIL=$((FAIL + 1)); echo "FAIL $d"; diff <(echo "$g") <(echo "$w") | head -20; fi
}
cmp_out "diff (patch)" diff
cmp_out "diff --stat" diff --stat
cmp_out "diff --numstat" diff --numstat
cmp_out "diff --name-status" diff --name-status
cmp_out "diff --name-only" diff --name-only
cmp_out "diff --shortstat" diff --shortstat
cmp_out "diff -U1" diff -U1 a.txt
cmp_out "status --porcelain" status --porcelain
cmp_out "status -s" status -s

for root in $G $Lw; do cd $root; [ $root = $G ] && git add -A && git mv move-me.txt moved.txt; [ $root = $Lw ] && l add -A && l mv move-me.txt moved.txt; done
cmp_out "diff --cached (patch with add, delete, rename, mode, binary)" diff --cached
cmp_out "diff --cached --name-status" diff --cached --name-status
cmp_out "diff --cached --stat" diff --cached --stat
cmp_out "status --porcelain with a rename" status --porcelain
cd $G; git commit -qm "second"; cd $Lw; l save -qm "second"
cmp_out "show HEAD:a.txt" show HEAD:a.txt
cmp_out "log --format=%s" log --format=%s
cmp_out "show --name-status --format=" show --name-status --format= HEAD
cmp_out "show --stat --format=%s" show --stat --format=%s HEAD
cmp_out "log --format=%an%x09%ae" log --format=%an%x09%ae
# The tree hash names the content: the same edits give the same hash in git and in layr,
# although git commits and layr states have different identities.
cmp_out "rev-parse HEAD^{tree}" rev-parse 'HEAD^{tree}'
cmp_out "log --format=%T" log --format=%T
cmp_out "log -1 --format=%T %t" log -1 '--format=%T %t'
cmp_out "rev-parse HEAD:d (a tree)" rev-parse HEAD:d
cmp_out "rev-parse HEAD:a.txt (a blob)" rev-parse HEAD:a.txt
cmp_out "rev-parse HEAD: (the root tree)" rev-parse HEAD:
cmp_out "rev-parse HEAD~1:gone.txt (a path of an older state)" rev-parse HEAD~1:gone.txt
git_missing=$(cd $G && git rev-parse HEAD:gone.txt 2>&1 >/dev/null)
eq "rev-parse head:gone.txt (a missing path) fails like git" "${git_missing//\'HEAD\'/\'head\'}" "$(cd $Lw && l rev-parse head:gone.txt 2>&1 >/dev/null)"
# An edit of the same size in the same second still reaches the exported tree: the exported
# index of the parent state has stat data for the file. Each round tries to stay in one second.
cd $Lw; stale=0
for i in $(seq 1 20); do
    printf "a$i" > racy.txt; l add racy.txt; l save -qm "racy a$i"; l rev-parse 'head^{tree}' >/dev/null
    printf "b$i" > racy.txt; l save -qam "racy b$i"
    [ "$(l rev-parse head:racy.txt)" = "$(printf "b$i" | git hash-object --stdin)" ] || stale=$((stale + 1))
done
eq "same-size edits in the same second are exported" 0 $stale
l reset --hard head~40 >/dev/null
for root in $G $Lw; do cd $root; printf 'trailing   \n' >> a.txt; done
cmp_out "diff --check" diff --check
finish
