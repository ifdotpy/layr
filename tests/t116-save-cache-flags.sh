#!/bin/bash
# ci: macos overlayfs
# `save --caches --cache a --cache b` saves every named cache path, not only the last one.
. "$(dirname "$0")/lib.sh"

sample $T/src
mkdir -p $T/src/node_modules/pkg && echo m > $T/src/node_modules/pkg/index.js
printf 'target/\n*.log\nnode_modules/\n' > $T/src/.gitignore
printf 'target regenerable\nnode_modules regenerable\n' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
cd $LAYR_ROOT/projects/p/lines/main
# init leaves ignored folders out: the build makes them in the line.
mkdir -p target/debug node_modules/pkg; echo bin > target/debug/x; echo m > node_modules/pkg/index.js
out=$(l save --caches --cache target --cache node_modules 2>&1)
has "target is saved" "target: overlay" "$out"
has "node_modules is saved" "node_modules: overlay" "$out"
has "the target overlay is saved" "saved " "$(l overlay list | grep 'folder=target')"
has "the node_modules overlay is saved" "saved " "$(l overlay list | grep 'folder=node_modules')"
l line new verify main >/dev/null 2>&1
eq "a new line receives the selected target bytes" "bin" "$(cat "$LAYR_ROOT/projects/p/lines/verify/target/debug/x" 2>/dev/null)"
eq "a new line receives the selected node_modules bytes" "m" "$(cat "$LAYR_ROOT/projects/p/lines/verify/node_modules/pkg/index.js" 2>/dev/null)"
out=$(l save --caches --cache target --cache nope 2>&1); rc=$?
eq "an unknown cache path fails" 1 "$rc"
has "the error names the unknown path" "'nope' is no cache path" "$out"
no "--cache without --caches fails" l save --cache target
finish
