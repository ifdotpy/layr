#!/bin/bash
# What common tools do at the edge of a folder overlay kept as a nested subvolume.
# Usage (root, after setup.sh): tools.sh <uid of a normal user>
# Makes $W/mnt/tools, $W/mnt/gitline and a second small file system $W/fs2.img, mounted
# without user_subvol_rm_allowed at $W2 (default /tmp/ovl-bench-mnt2, a path the user can
# reach), for the tests as a normal user.
set -u
U=${1:?usage: tools.sh <uid of a normal user>}
W=${W:-/root/ovl-bench}
W2=${W2:-/tmp/ovl-bench-mnt2}
M=$W/mnt
say() { printf '%-64s %s\n' "$1" "$2"; }
first_extent() { filefrag -v "$1" 2>/dev/null | awk '$1 == "0:" { print $4; exit }'; }
# "reflink" when both files start at the same physical extent, else "copy".
shared() { a=$(first_extent "$1"); b=$(first_extent "$2"); [ -n "$a" ] && [ "$a" = "$b" ] && echo reflink || echo copy; }
errno_of() { python3 -c "
import os, sys
try:
    getattr(os, sys.argv[1])(sys.argv[2], sys.argv[3]); print('ok')
except OSError as e:
    import errno; print(errno.errorcode[e.errno])" "$@"; }
is_subvol() { [ "$(stat -c %i "$1" 2>/dev/null)" = 256 ] && echo subvolume || { [ -d "$1" ] && echo "plain folder" || echo missing; }; }
rnd() { head -c 1048576 /dev/urandom >"$1"; }

# A line with a folder overlay `sec` (nested subvolume) and 1 MiB files (not inline).
L=$M/tools
btrfs -q subvolume create "$L"
cd "$L" || exit 1
mkdir d
for f in a b c e g d/x; do rnd "$f.bin"; done
btrfs -q subvolume create sec
echo s >sec/s.txt
rnd sec/y.bin
btrfs -q subvolume snapshot -r "$L" "$M/tools-ref" # keeps the original extents
R=$M/tools-ref

say "st_dev of the line, of the folder overlay" "$(stat -c %d .), $(stat -c %d sec)"
say "rename(2) line -> overlay" "$(errno_of rename a.bin sec/a.bin)"
say "rename(2) overlay -> line" "$(errno_of rename sec/y.bin y.bin)"
say "link(2) line -> overlay" "$(errno_of link a.bin sec/a.bin)"
say "rename(2) of the overlay folder in the line (sec -> sec2)" "$(errno_of rename sec sec2)"
[ -d sec2 ] && mv sec2 sec
say "FICLONE line -> overlay (cp --reflink=always)" "$(cp --reflink=always b.bin sec/b.bin && echo "exit 0, $(shared "$R/b.bin" sec/b.bin)")"
say "cp (GNU $(cp --version | head -1 | awk '{print $NF}'), default)" "$(cp b.bin sec/b2.bin && echo "exit 0, $(shared "$R/b.bin" sec/b2.bin)")"
say "cp (uutils, default)" "$(/usr/lib/cargo/bin/coreutils/cp b.bin sec/b3.bin && echo "exit 0, $(shared "$R/b.bin" sec/b3.bin)")"
say "mv file (GNU)" "$(mv a.bin sec/ && echo "exit 0, $(shared "$R/a.bin" sec/a.bin)")"
say "mv file (uutils)" "$(/usr/lib/cargo/bin/coreutils/mv c.bin sec/ && echo "exit 0, $(shared "$R/c.bin" sec/c.bin)")"
say "mv folder (GNU)" "$(mv d sec/ && echo "exit 0, $(shared "$R/d/x.bin" sec/d/x.bin)")"
say "Python shutil.move" "$(python3 -c 'import shutil; shutil.move("e.bin", "sec/e.bin")' && echo "exit 0, $(shared "$R/e.bin" sec/e.bin)")"
say "Python shutil.copy2" "$(python3 -c 'import shutil; shutil.copy2("g.bin", "sec/g.bin")' && echo "exit 0, $(shared "$R/g.bin" sec/g.bin)")"
say "editor save in the overlay (temp file in sec, fsync, rename)" "$(python3 -c '
import os
with open("sec/.s.txt.tmp", "w") as f:
    f.write("saved\n"); f.flush(); os.fsync(f.fileno())
os.replace("sec/.s.txt.tmp", "sec/s.txt")' && echo "exit 0, $(cat sec/s.txt)")"
say "editor save with the temp file in the line root" "$(errno_of rename "$(echo new >.tmp-s && echo .tmp-s)" sec/s.txt)"
say "sed -i in the overlay" "$(sed -i 's/saved/sed/' sec/s.txt && echo "exit 0, $(cat sec/s.txt)")"
say "vim :wq in the overlay" "$(vim -Nu NONE -i NONE -es -c 'normal Avim' -c wq sec/s.txt && echo "exit 0, $(cat sec/s.txt), $(is_subvol sec)")"
say "files: find . / find -xdev ." "$(find . -type f | wc -l) / $(find . -xdev -type f | wc -l)"
btrfs -q subvolume snapshot -r "$L" "$M/tools-snap"
say "snapshot of the line: sec is" "$(is_subvol "$M/tools-snap/sec"), $(find "$M/tools-snap/sec" -mindepth 1 | wc -l) entries, inode $(stat -c %i "$M/tools-snap/sec")"
say "send --no-data of that snapshot: lines naming sec/" "$(btrfs send -q --no-data "$M/tools-snap" | btrfs receive --dump | grep -c '/sec/')"
btrfs -q subvolume snapshot "$M/tools-snap" "$M/tools-w"
say "rmdir of that empty placeholder in a writable snapshot" "$(rmdir "$M/tools-w/sec" && echo ok)"

# git in a line whose folder `sec` is a nested subvolume.
G=$M/gitline
btrfs -q subvolume create "$G"
cd "$G" || exit 1
git() { command git -c user.name=u -c user.email=u@example.com -c init.defaultBranch=main "$@"; }
git init -q .
echo a >a.txt
btrfs -q subvolume create sec
echo s >sec/s.txt
echo t >sec/t.txt
git add -A && git commit -qm one
say "git status after the commit" "[$(git status --porcelain)]"
echo more >>sec/s.txt
say "git status after an edit in sec" "[$(git status --porcelain)]"
git commit -qam two
git checkout -qb other
echo other >sec/s.txt
rm sec/t.txt
echo n >sec/n.txt
git add -A && git commit -qm other
git checkout -q main
say "git checkout main from other: sec/ files, sec is" "$(ls sec | tr '\n' ' ')($(cat sec/s.txt | tr '\n' ' ')), $(is_subvol sec)"
echo x >>sec/s.txt
git stash -q && say "git stash / stash pop in sec" "$(git stash pop -q && git status --porcelain), $(is_subvol sec)"
git checkout -q -- sec/s.txt
say "git mv a.txt sec/" "$(git mv a.txt sec/ 2>&1 | head -1; echo "exit ${PIPESTATUS[0]}")"
git checkout -qb nosec
git rm -rq sec && git commit -qm "no sec"
say "git rm -r sec (as root): sec is" "$(is_subvol sec)"
git checkout -q main
say "git checkout main again: sec is" "$(is_subvol sec)"
mv sec sec.old && btrfs -q subvolume create sec && cp -a --reflink=always sec.old/. sec/ && rm -rf sec.old
say "sec made a subvolume again, git status" "[$(git status --porcelain)], $(is_subvol sec)"
git checkout -q nosec
say "git checkout of a commit without sec: sec is" "$(is_subvol sec)"

# As a normal user, on a file system without user_subvol_rm_allowed.
cd / || exit 1
truncate -s 512M "$W/fs2.img"
mkfs.btrfs -q -f "$W/fs2.img"
mkdir -p "$W2"
mount -o loop "$W/fs2.img" "$W2"
chown "$U" "$W2"
as_user() { setpriv --reuid="$U" --regid="$(id -g "$U")" --clear-groups env HOME=/tmp "$@"; }
as_user btrfs -q subvolume create "$W2/line" 2>/dev/null || { btrfs -q subvolume create "$W2/line"; chown "$U" "$W2/line"; }
as_user btrfs -q subvolume create "$W2/line/sec"
as_user sh -c "echo s >'$W2/line/sec/s.txt'"
say "user: rm -rf of a folder overlay it owns" "$(as_user rm -rf "$W2/line/sec" 2>&1 | head -1)$(is_subvol "$W2/line/sec")"
as_user btrfs -q subvolume create "$W2/line/sec2"
say "user: btrfs subvolume delete of it" "$(as_user btrfs -q subvolume delete "$W2/line/sec2" 2>&1 | head -1)"
umount "$W2"
rmdir "$W2"
rm -f "$W/fs2.img"
