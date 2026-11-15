#!/bin/bash
# ci: macos overlayfs
# `layr init --from-git` of a checkout whose .gitattributes converts line endings (`eol=crlf`): git checks
# the file out with CRLF and `git status` shows it clean, so it is no unsaved change. The project gets no
# automatic state for it, the line keeps the blob bytes, and init reports the normalized path. A real edit of
# such a file is refused as uncommitted, as any edit is. Another filter (as Git LFS: the blob is not the file)
# is no line ending: its content on disk is saved as the folder's change, never replaced by the blob.
. "$(dirname "$0")/lib.sh"
B=$LAYR_ROOT
export LAYR_ROOT=$B/store
mkdir -p "$LAYR_ROOT"
umask 022
DB() { # DB <project> <sql>
    python3 -I -c 'import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); [print("|".join(str(v) for v in r)) for r in c.execute(sys.argv[2])]' \
        "$LAYR_ROOT/projects/$1/meta/db/layr.sqlite" "$2"
}
git init -q "$B/up"
cd "$B/up" || exit 1
git config user.email t@example.invalid && git config user.name t
printf '*.bat text eol=crlf\n' > .gitattributes
printf 'echo one\necho two\n' > run.bat
echo plain > a.txt
git add . && git commit -qm "files" && git branch -M main
git clone -q "$B/up" "$B/co"
ok "git checks the batch file out with CRLF" grep -q $'\r' "$B/co/run.bat"
eq "and git calls the checkout clean" "" "$(git -C "$B/co" status --porcelain)"
out=$("$LAYR" init cp --from-git "$B/co" 2>&1); rc=$?
eq "a project is made from the checkout" 0 "$rc"
has "init reports the normalized path" "1 path(s) differ from their Git blobs only by line endings" "$out"
eq "no automatic state is made" 0 "$(DB cp "SELECT count(*) FROM states WHERE kind != 'import'")"
cd "$B/co" || exit 1
eq "the line keeps the blob bytes" "$(printf 'echo one\necho two\n')" "$(cat run.bat)"
no "the line folder holds no CRLF" grep -q $'\r' run.bat
eq "layr has nothing to save" "" "$(l status --short 2>&1)"

# A real edit of a converted file is an uncommitted change: init refuses it, as any edit.
git clone -q "$B/up" "$B/co2"
printf 'echo three\r\n' >> "$B/co2/run.bat"
no "an edit of a converted file is refused" "$LAYR" init cq --from-git "$B/co2"

# A clean/smudge filter other than line endings: git calls the checkout clean, but the bytes on disk are not
# the blob with CRLF. The content on disk stays.
git init -q "$B/up3"
cd "$B/up3" || exit 1
git config user.email t@example.invalid && git config user.name t
printf '*.up filter=up\n' > .gitattributes
echo lower > x.up
git add . && git commit -qm "filtered" && git branch -M main
git clone -q -c filter.up.smudge='tr a-z A-Z' -c filter.up.clean='tr A-Z a-z' "$B/up3" "$B/co3"
git -C "$B/co3" config filter.up.smudge 'tr a-z A-Z' && git -C "$B/co3" config filter.up.clean 'tr A-Z a-z'
eq "the filtered file is smudged on disk" "LOWER" "$(cat "$B/co3/x.up")"
eq "and git calls that checkout clean" "" "$(git -C "$B/co3" status --porcelain)"
out=$("$LAYR" init cf --from-git "$B/co3" 2>&1); rc=$?
eq "a project is made from the filtered checkout" 0 "$rc"
no "a filtered file is not reported as a line ending" grep -q "only by line endings" <<< "$out"
eq "the content on disk stays in the line" "LOWER" "$(cat "$B/co3/x.up")"

finish
