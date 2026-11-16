#!/bin/bash
# ci: macos overlayfs
# Replacing a cache root loses provenance. A save refuses unknown bytes, preserving the folder.
. "$(dirname "$0")/lib.sh"
sample "$T/src"
l init g --from "$T/src" -q >/dev/null
P=$LAYR_ROOT/projects/g/lines
cd "$P/main"
l config regenerable '["target"]'
l line new -q w >/dev/null
cd "$P/w"
echo one >> notes.txt
l save -q
rm -rf target; mkdir -p target; echo big > target/x
out=$(l save -q 2>&1); rc=$?
no "a recreated cache with unknown bytes is refused" test "$rc" = 0
has "the refusal keeps output intact" 'output left intact' "$out"
eq "the file remains in the recreated folder" big "$(cat target/x)"
no "a save never stamps the unknown replacement" test -e target/.layr-cache-location
rm -rf target; mkdir -p target; echo big2 > target/y
out=$(l save --overlays -q 2>&1); rc=$?
no "an explicit overlay save also refuses unknown replacement output" test "$rc" = 0
eq "the second file is kept" big2 "$(cat target/y)"
no "the second replacement is not stamped" test -e target/.layr-cache-location
finish
