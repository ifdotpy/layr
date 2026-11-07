#!/bin/bash
# ci: macos-plain
# LayrFS tool substitution: `layr tool rg|find` (and links named rg/find) answer from the index of
# the server with the output of the real tools, see the changes made just before each call, and
# fall back to the real tool for what they do not support. Needs rg on PATH.
. "$(dirname "$0")/lib.sh"

command -v rg >/dev/null || { echo "FATAL: rg (ripgrep) is not on PATH" >&2; exit 1; }
export LAYR_FS_SOCKET=$T/fs.sock LAYR_FS_STRICT=1
unset LAYR_FS_OFF LAYR_TOOL_ACTIVE RIPGREP_CONFIG_PATH

W=$T/work
mkdir -p "$W/src/deep" "$W/docs" "$W/.hidden" "$W/target/debug" "$W/vendor/lib"
cd "$W" || exit 1
git init -q
printf 'fn main() {\n    println!("hello");\n}\n// TODO: one\n' > src/main.rs
printf 'pub fn lib() {}\n// todo: two\nfn helper() {}\n' > src/lib.rs
printf 'fn deep() {}\n' > src/deep/x.rs
printf '# Title\nfn in docs\nTODO three\n' > docs/guide.md
printf 'fn hidden() {}\n' > .hidden/h.rs
printf 'fn built() {}\n' > target/debug/out.rs
printf 'fn vendored() {}\n' > vendor/lib/v.rs
printf 'a\0b fn binary\n' > data.bin
printf 'target/\n*.log\n' > .gitignore
printf 'vendor/\n' > .ignore
printf 'fn log() {}\n' > run.log

eq "the index is built on request" "ready" "$("$LAYR" fs warm --wait)"
has "status lists the tree" "$W" "$("$LAYR" fs status)"

# same <description> <dir> <args...>: `layr tool rg` prints what `rg --sort path` prints.
same() {
    local desc=$1 dir=$2
    shift 2
    local want got
    want=$(cd "$dir" && rg --sort path "$@" </dev/null 2>&1; echo "exit $?")
    got=$(cd "$dir" && "$LAYR" tool rg "$@" </dev/null 2>&1; echo "exit $?")
    eq "rg $desc" "$want" "$got"
}

# same_find <description> <dir> <args...>: `layr tool find` prints the lines of `find`, sorted.
same_find() {
    local desc=$1 dir=$2
    shift 2
    local want got
    want=$(cd "$dir" && find "$@" </dev/null 2>&1 | LC_ALL=C sort; echo "exit ${PIPESTATUS[0]}")
    got=$(cd "$dir" && "$LAYR" tool find "$@" </dev/null 2>&1 | LC_ALL=C sort; echo "exit ${PIPESTATUS[0]}")
    eq "find $desc" "$want" "$got"
}

check_all() {
    same "pattern" "$W" fn
    same "files" "$W" --files
    same "count" "$W" -c fn
    same "files with matches" "$W" -l TODO
    same "files without match" "$W" --files-without-match TODO
    same "ignore case and line numbers" "$W" -in todo
    same "smart case" "$W" -S todo
    same "fixed strings and word" "$W" -Fw 'fn'
    same "context" "$W" -C1 TODO
    same "after context with max count" "$W" -A1 -m1 fn
    same "only matching" "$W" -o 'fn [a-z]+'
    same "invert in one file" "$W" -v fn src/lib.rs
    same "several patterns" "$W" -e TODO -e todo
    same "a folder argument" "$W" fn src
    same "dot argument" "$W" -n fn .
    same "an ignored file named explicitly" "$W" fn run.log
    same "an explicit binary file" "$W" fn data.bin
    same "type filter" "$W" -t rust fn
    same "type negation" "$W" -T rust fn
    same "negated glob" "$W" -g '!src/deep' fn
    same "no match" "$W" no-such-text-anywhere
    same "from a subfolder" "$W/src" -n fn
    same "files from a subfolder" "$W/src" --files
    same_find "everything" "$W" .
    same_find "files" "$W" . -type f
    same_find "folders" "$W" . -type d
    same_find "by name" "$W" . -name '*.rs'
    same_find "by path, negated" "$W" . -type f -not -path './target/*'
    same_find "depth" "$W" src -maxdepth 1
    same_find "case-insensitive name" "$W" . -iname 'GUIDE*'
}
check_all

# Changes immediately before a call are seen (the watcher barrier).
mkdir -p src/new/inner && printf 'fn added() {}\n' > src/new/inner/a.rs
rm src/deep/x.rs
mv docs manual
printf 'fn root() {}\n' > top.rs
printf 'target/\n*.log\n*.md\n' > .gitignore
eq "a new file is found at once" "src/new/inner/a.rs:fn added() {}" "$("$LAYR" tool rg added </dev/null)"
check_all
same "a renamed folder" "$W" fn manual
rm .gitignore
same "a removed ignore file" "$W" --files
printf 'target/\n' > .gitignore
same "a restored ignore file" "$W" --files

# Links named after the tools work like `layr tool`.
mkdir -p "$T/bin" && ln -s "$LAYR" "$T/bin/rg" && ln -s "$LAYR" "$T/bin/find"
want=$(rg --sort path -n fn </dev/null)
eq "a link named rg" "$want" "$(PATH=$T/bin:$PATH rg -n fn </dev/null)"
want=$(find . -name '*.rs' | LC_ALL=C sort)
eq "a link named find" "$want" "$(PATH=$T/bin:$PATH find . -name '*.rs' </dev/null | LC_ALL=C sort)"

# Unsupported input is not served; without LAYR_FS_STRICT the real tool answers.
no "a whitelist glob is not served" "$LAYR" tool rg -g '*.rs' fn </dev/null
no "--hidden is not served" "$LAYR" tool rg --hidden fn </dev/null
no "a hidden folder is not served" "$LAYR" tool rg fn .hidden </dev/null
no "an ignored folder is not served" "$LAYR" tool rg fn target </dev/null
no "find -exec is not served" "$LAYR" tool find . -exec true \; </dev/null
no "standard input for rg is not served" sh -c "echo fn | '$LAYR' tool rg fn"
eq "the real rg answers a fallback" "$(rg --hidden hidden </dev/null | LC_ALL=C sort)" \
    "$(LAYR_FS_STRICT= "$LAYR" tool rg --hidden hidden </dev/null | LC_ALL=C sort)"
eq "the real rg reads standard input" "fn" "$(echo fn | LAYR_FS_STRICT= "$LAYR" tool rg fn)"
eq "outside a git work tree the real find answers" "$T/bin/rg" \
    "$(cd "$T/bin" && LAYR_FS_STRICT= "$LAYR" tool find "$T/bin" -name rg </dev/null)"

"$LAYR" fs stop
no "the server stopped" "$LAYR" fs status

# A socket in an existing folder leaves the folder's mode alone; a non-socket file is kept.
mkdir -p "$T/shared" && chmod 755 "$T/shared"
eq "a server in an existing folder starts" "ready" "$(LAYR_FS_SOCKET=$T/shared/fs.sock "$LAYR" fs warm --wait)"
eq "the existing folder keeps its mode" "755" "$(stat_field mode "$T/shared")"
LAYR_FS_SOCKET=$T/shared/fs.sock "$LAYR" fs stop
echo keep > "$T/shared/not-a-socket"
no "a file in place of the socket is not replaced" env LAYR_FS_SOCKET=$T/shared/not-a-socket "$LAYR" fs warm --wait
eq "the file is kept" "keep" "$(cat "$T/shared/not-a-socket")"
finish
