# A credential-free, preinstalled Rust toolchain and offline dependencies.
# This file is sourced by each check in its fresh HOME.
set -euo pipefail
seed=${LAYR_CHECK_SEED:-/usr/local/share/layr-checks}
test -d "$seed/cargo" && test -d "$seed/rustup" || {
    echo 'Prepare the offline toolchain at /usr/local/share/layr-checks (docs/checks.md)' >&2
    exit 1
}
export CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$seed/rustup"
mkdir -p "$CARGO_HOME"
if [ "$(uname -s)" = Darwin ]; then
    # The sandbox can read this prepared seed but cannot change it. Keep Cargo's
    # locks in the private HOME without copying unrelated cached crate files.
    ln -s "$seed/cargo/registry" "$CARGO_HOME/registry"
    ln -s "$seed/cargo/bin" "$CARGO_HOME/bin"
else
    cp -a --no-preserve=ownership "$seed/cargo/." "$CARGO_HOME/"
    chmod -R u+rwX "$CARGO_HOME"
fi
export PATH="$CARGO_HOME/bin:/usr/local/sbin:/usr/sbin:/sbin:$PATH" CARGO_NET_OFFLINE=true
if [ "$(uname -s)" = Linux ]; then
    # One rustc job per CPU, at most one per 2 GiB of memory; one shell test per CPU.
    cpus=$(nproc)
    memory_jobs=$(( $(awk '/^MemTotal:/ {print $2}' /proc/meminfo) / 2097152 ))
    # In Cargo's config, not the environment: retest doctor reports a shell's CARGO_* variables.
    grep -qs '^\[build\]' "$CARGO_HOME/config.toml" ||
        printf '[build]\njobs = %s\n' $(( memory_jobs < 1 ? 1 : memory_jobs < cpus ? memory_jobs : cpus )) >> "$CARGO_HOME/config.toml"
    export LAYR_TEST_JOBS=${LAYR_TEST_JOBS:-$cpus}
fi
if [ "$(uname -s)" = Darwin ]; then
    prepared_toolchain=$(cat "$seed/toolchain")
    toolchain=$prepared_toolchain
    # A state that pins Rust gets that version; a state without rust-toolchain.toml gets the prepared one.
    if [ -e rust-toolchain.toml ]; then
        channel=$(sed -n 's/^channel = "\([0-9][0-9.]*\)"$/\1/p' rust-toolchain.toml)
        test -n "$channel" && test "$(printf '%s\n' "$channel" | wc -l | tr -d ' ')" = 1 || {
            echo 'The state must pin one stable Rust version' >&2
            exit 1
        }
        toolchain="$channel-${prepared_toolchain#*-}"
        test -x "$RUSTUP_HOME/toolchains/$toolchain/bin/rustc" || {
            echo "Prepare Rust $toolchain in $seed" >&2
            exit 1
        }
    fi
    export PATH="$RUSTUP_HOME/toolchains/$toolchain/bin:$PATH"
    # Bypass xcrun's per-user cache writes outside the sandbox's private HOME.
    export CC="$(cat "$seed/clang")" SDKROOT="$(cat "$seed/sdk")"
    export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$CC"
    export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$CC"
    # Git locates its helpers relative to its actual binary. A relocated symlink
    # and Apple's /usr/bin shim both need paths outside this private environment.
    export PATH="$(dirname "$(cat "$seed/git")"):$PATH"
    # Layr's Git child switches to the caller's OS home. Its selected environment
    # explicitly preserves this private config path instead of reading that home.
    export GIT_CONFIG_GLOBAL="$HOME/.gitconfig"
    : > "$GIT_CONFIG_GLOBAL"
fi
if [ "$(id -u)" = 0 ]; then
    # A privileged check copy lives at /var/tmp/layr-check-<id>: below it the Unix sockets of the
    # tests get paths longer than SUN_LEN.
    TMPDIR=$(mktemp -d /var/tmp/layr-root-tmp.XXXXXX)
    # Tests run commands as other users (an owner) below it.
    chmod 755 "$TMPDIR"
    if [ "$(uname -s)" = Linux ] && [ -z "${LAYR_CHECK_TMP_ON_DISK:-}" ]; then
        # The suite waits on the disk, not the CPUs: its tests get a Btrfs in memory. The check's
        # private mount namespace (checks.rs) drops it, and its loop device, with the check.
        half=$(( $(awk '/^MemTotal:/ {print $2}' /proc/meminfo) * 512 ))
        # A check's file size limit (checks.memory) bounds the image too.
        fsize=$(awk '/^Max file size/ {print $4}' /proc/self/limits)
        [ "$fsize" != unlimited ] && [ "$half" -gt "$fsize" ] && half=$(( fsize - 1048576 ))
        mount -t tmpfs -o size=$half,mode=755 layr-check-tmp "$TMPDIR"
        truncate -s $half "$TMPDIR/btrfs.img"
        mkfs.btrfs -q "$TMPDIR/btrfs.img"
        mkdir "$TMPDIR/fs"
        mount -o loop "$TMPDIR/btrfs.img" "$TMPDIR/fs"
        chmod 755 "$TMPDIR/fs"
        TMPDIR=$TMPDIR/fs
    fi
    # Git refuses folders of another owner to root; the test stores are such folders.
    export GIT_CONFIG_GLOBAL="$HOME/.gitconfig"
    git config --file "$GIT_CONFIG_GLOBAL" --add safe.directory '*'
elif [ "$(uname -s)" = Darwin ] && [ -n "${LAYR_CHECK_TMPDIR:-}" ]; then
    TMPDIR="$LAYR_CHECK_TMPDIR"
    export LAYR_TEST_GATEWAY_UNIX=1
    mkdir -p "$TMPDIR"
else
    TMPDIR="$PWD/.check-tmp"
    mkdir -p "$TMPDIR"
fi
export TMPDIR
