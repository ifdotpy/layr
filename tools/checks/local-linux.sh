#!/bin/bash
# Run one Linux check of this folder as a CI runner runs it, before the queue: the tools of
# tools/checks/install-linux.sh, root, loop devices, and a btrfs TMPDIR on a loop file, like the runner's
# /var/tmp/layr-root-tmp.*/fs. Without this, a failure that only CI saw (another TMPDIR, no bubblewrap, no
# npm, a store on its own file system) cost one queue run each (2026-11-17).
#   tools/checks/local-linux.sh unit | integration | overlayfs ext4 | overlayfs xfs
#   LAYR_LOCAL_CI_TESTS="t01-basic t02-merge" tools/checks/local-linux.sh integration   only these shell tests
# Needs Docker with --privileged. LAYR_LOCAL_CI_HOST=<ssh host> copies this folder there and runs it there.
# The first run builds the image layr-local-ci (install-linux.sh, about 10 minutes); a change of
# install-linux.sh builds it again. The build output stays in the Docker volume layr-local-ci-target.
set -euo pipefail
check=${1:?usage: local-linux.sh unit|integration|overlayfs <ext4|xfs>}
shift
case "$check" in unit|integration|overlayfs) ;; *) echo "unknown check $check" >&2; exit 2 ;; esac
if [ -n "${LAYR_LOCAL_CI_HOST:-}" ]; then
    remote_dir=.cache/layr-local-ci/src
    ssh "$LAYR_LOCAL_CI_HOST" "mkdir -p $remote_dir"
    COPYFILE_DISABLE=1 tar --exclude=./target --exclude=./.git -cf - . |
        ssh "$LAYR_LOCAL_CI_HOST" "rm -rf $remote_dir && mkdir -p $remote_dir && tar -xf - -C $remote_dir 2>/dev/null"
    exec ssh "$LAYR_LOCAL_CI_HOST" "cd $remote_dir && LAYR_LOCAL_CI_TESTS='${LAYR_LOCAL_CI_TESTS:-}' bash tools/checks/local-linux.sh $check $*"
fi
tag=layr-local-ci:$(sha256sum tools/checks/install-linux.sh | cut -c1-12)
if ! docker image inspect "$tag" > /dev/null 2>&1; then
    echo "building $tag (install-linux.sh)"
    name=layr-local-ci-build-$$
    docker run --name "$name" --privileged -v "$PWD:/src:ro" ubuntu:24.04 bash -c '
        set -e
        apt-get update -qq && apt-get install -y -qq curl ca-certificates > /dev/null
        mkdir /work && tar -C /src --exclude=./target -cf - . | tar -C /work -xf -
        cd /work && bash tools/checks/install-linux.sh && rm -rf /work'
    docker commit "$name" "$tag" > /dev/null
    docker rm "$name" > /dev/null
fi
exec docker run --rm --privileged -v /dev:/dev -v "$PWD:/src:ro" -v layr-local-ci-target:/work/target \
    -e LAYR_LOCAL_CI_TESTS="${LAYR_LOCAL_CI_TESTS:-}" "$tag" bash -c '
    set -euo pipefail
    mkdir -p /work && tar -C /src --exclude=./target -cf - . | tar -C /work -xf -
    # The runner gives each check a btrfs TMPDIR of its own.
    truncate -s 40G /var/tmp/layr-ci.img && mkfs.btrfs -q -f /var/tmp/layr-ci.img
    mkdir -p /var/tmp/layr-root-tmp/fs && mount -o loop /var/tmp/layr-ci.img /var/tmp/layr-root-tmp/fs
    export TMPDIR=/var/tmp/layr-root-tmp/fs HOME=/root
    cd /work
    if [ -n "$LAYR_LOCAL_CI_TESTS" ]; then
        . tools/checks/environment.sh
        cargo build --locked --offline $( [ "$0" = overlayfs ] && echo --features overlayfs )
        export LAYR="$PWD/target/debug/layr"
        exec bash tests/run.sh $LAYR_LOCAL_CI_TESTS
    fi
    exec bash "tools/checks/$0.sh" "$@"' "$check" "$@"
