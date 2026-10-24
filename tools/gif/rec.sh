#!/bin/bash
# Runs the steps of one GIF scenario in a fresh btrfs at /srv/layr and prints each command with
# its real output between markers. Usage: rec.sh <steps-file>. Lines starting with "#" are
# hidden setup; other lines are shown as typed commands.
set -u
export PATH=${LAYR_BIN_DIR:?set LAYR_BIN_DIR to the folder with the layr binary}:$PATH
export LAYR_ROOT=/srv/layr LAYR_AUTHOR_NAME=alice LAYR_AUTHOR_EMAIL=alice@acme.dev
export GIT_AUTHOR_NAME=alice GIT_AUTHOR_EMAIL=alice@acme.dev GIT_COMMITTER_NAME=alice GIT_COMMITTER_EMAIL=alice@acme.dev
umount /srv/layr 2>/dev/null; rm -rf /srv/layr /srv/layr.img /srv/vault /srv/src /root/paper.txt
truncate -s 4G /srv/layr.img && mkfs.btrfs -q -f /srv/layr.img && mkdir -p /srv/layr && mount -o loop,user_subvol_rm_allowed /srv/layr.img /srv/layr
mkdir -p /srv/src/src && cd /srv/src
printf 'pub fn parse(s: &str) -> Vec<&str> {\n    s.split(",").collect()\n}\n' > src/parser.rs
printf 'mod parser;\nfn main() {}\n' > src/main.rs
printf 'test:\n\ttest -f src/parser.rs\n' > Makefile
printf '# shop\n' > README.md
layr init shop --from /srv/src -q >/dev/null 2>&1
cd /srv/layr/projects/shop/lines/main
layr protect main --remove >/dev/null 2>&1
while IFS= read -r line; do
    case "$line" in
        "#"*) eval "${line#\#}" >/dev/null 2>&1 ;;
        "") ;;
        *) echo "@@CMD $(printf '%s' "$line" | base64 -w0)"
           eval "$line" 2>&1
           echo "@@END" ;;
    esac
done < "$1"
