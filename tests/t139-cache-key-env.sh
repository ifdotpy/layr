#!/bin/bash
# ci: macos overlayfs
# The cache key holds the environment that changes a build (docs/cache-overlays.md, section 6): a line
# takes a cache only when these values match, and a line that takes none names the parts that differ.
# The values that the project pins (`[workspace.metadata.layr-retest] env`) win over the caller's, and
# the project setting `cache.key-env` adds names.
. "$(dirname "$0")/lib.sh"

sample $T/src
printf '[package]\nname = "x"\nversion = "0.1.0"\n\n[workspace]\n' > $T/src/Cargo.toml
echo 'target regenerable' > $T/src/.layrattributes
l init p --from $T/src -q >/dev/null
P=$LAYR_ROOT/projects/p/lines
cd $P/main || exit 1
l protect main --remove >/dev/null 2>&1 || true
rm -rf target; mkdir target; echo built > target/out
MACOSX_DEPLOYMENT_TARGET=11.0 l save --glasses -q

MACOSX_DEPLOYMENT_TARGET=11.0 l line new same main >/dev/null 2>&1
eq "the same environment: warm" built "$(cat $P/same/target/out 2>/dev/null)"
err=$(MACOSX_DEPLOYMENT_TARGET=12.0 l line new other main 2>&1 >/dev/null)
no "another deployment target: cold" test -e $P/other/target/out
has "the line names the part that differs" "the key differs in env:MACOSX_DEPLOYMENT_TARGET" "$err"

# The project pins the value: the caller's value does not matter.
printf '[package]\nname = "x"\nversion = "0.1.0"\n\n[workspace]\n\n[workspace.metadata.layr-retest]\nenv = { MACOSX_DEPLOYMENT_TARGET = "11.0" }\n' > Cargo.toml
l save -a -m pin -q
MACOSX_DEPLOYMENT_TARGET=13.0 l save --glasses -q
MACOSX_DEPLOYMENT_TARGET=14.0 l line new pinned main >/dev/null 2>&1
eq "a pinned value wins over the caller's" built "$(cat $P/pinned/target/out 2>/dev/null)"

# cache.key-env adds a name.
l config cache.key-env '["LAYR_TEST_BUILD_MODE"]' >/dev/null
LAYR_TEST_BUILD_MODE=a l save --glasses -q
LAYR_TEST_BUILD_MODE=a l line new mode-a main >/dev/null 2>&1
eq "the added name with the same value: warm" built "$(cat $P/mode-a/target/out 2>/dev/null)"
err=$(LAYR_TEST_BUILD_MODE=b l line new mode-b main 2>&1 >/dev/null)
no "the added name with another value: cold" test -e $P/mode-b/target/out
has "it names the added part" "env:LAYR_TEST_BUILD_MODE" "$err"
finish
