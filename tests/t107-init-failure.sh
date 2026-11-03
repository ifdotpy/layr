#!/bin/bash
# ci: macos overlayfs
# A failed init leaves no project folder. init never uses a project folder that exists.
. "$(dirname "$0")/lib.sh"

SRC=$T/src
mkdir -p $SRC
echo hello > $SRC/a.txt

# 1. The source is missing: init fails and the project folder is absent.
no "init from a missing folder fails" l init new --from $T/missing -q
ok "the project folder of the failed init is absent" test ! -e $LAYR_ROOT/projects/new

# 2. A project folder with a sentinel file exists: init refuses it and changes nothing in it.
mkdir -p $LAYR_ROOT/projects/x
echo keep > $LAYR_ROOT/projects/x/sentinel.txt
before=$(cd $LAYR_ROOT/projects/x && find . | sort)
no "init refuses an existing project folder" l init x --from $SRC -q
eq "the sentinel file of the existing folder is unchanged" keep "$(cat $LAYR_ROOT/projects/x/sentinel.txt)"
eq "the existing folder listing is unchanged" "$before" "$(cd $LAYR_ROOT/projects/x && find . | sort)"

# 3. A partial folder without a database (only an empty meta folder) is refused the same way.
mkdir -p $LAYR_ROOT/projects/y/meta
echo keep > $LAYR_ROOT/projects/y/sentinel.txt
before=$(cd $LAYR_ROOT/projects/y && find . | sort)
no "init refuses a partial project folder" l init y --from $SRC -q
eq "the sentinel file of the partial folder is unchanged" keep "$(cat $LAYR_ROOT/projects/y/sentinel.txt)"
eq "the partial folder listing is unchanged" "$before" "$(cd $LAYR_ROOT/projects/y && find . | sort)"

# 4. The line step fails (an invalid line name is checked when the line is made): the project
# folder that this init created is removed.
no "init fails on an invalid line name" l init z --from $SRC --line "bad name" -q
ok "the project folder of the failed line step is absent" test ! -e $LAYR_ROOT/projects/z

finish
