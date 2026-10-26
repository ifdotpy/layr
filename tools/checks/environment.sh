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
if [ "$(uname -s)" = Darwin ]; then
    toolchain=$(cat "$seed/toolchain")
    export PATH="$RUSTUP_HOME/toolchains/$toolchain/bin:$PATH"
    # Bypass xcrun's per-user cache writes outside the sandbox's private HOME.
    export CC="$(cat "$seed/clang")" SDKROOT="$(cat "$seed/sdk")"
    export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$CC"
    export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$CC"
fi
export TMPDIR="$PWD/.check-tmp"
mkdir -p "$TMPDIR"
