# A credential-free, preinstalled Rust toolchain and offline dependencies.
# This file is sourced by each check in its fresh HOME.
set -euo pipefail
seed=/usr/local/share/layr-checks
test -d "$seed/cargo" && test -d "$seed/rustup" || {
    echo 'Prepare the offline toolchain at /usr/local/share/layr-checks (docs/checks.md)' >&2
    exit 1
}
export CARGO_HOME="$HOME/.cargo" RUSTUP_HOME="$seed/rustup"
mkdir -p "$CARGO_HOME"
cp -a --no-preserve=ownership "$seed/cargo/." "$CARGO_HOME/"
chmod -R u+rwX "$CARGO_HOME"
export PATH="$CARGO_HOME/bin:$PATH" CARGO_NET_OFFLINE=true
export TMPDIR="$PWD/.check-tmp"
mkdir -p "$TMPDIR"
