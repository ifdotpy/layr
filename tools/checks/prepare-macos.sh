#!/bin/bash
# Run once as the Mac runner user, outside the check sandbox. Does not use sudo.
set -euo pipefail
test "$(uname -s)" = Darwin
test "$(id -u)" != 0
base="$HOME/Library/Application Support/Layr"
mkdir -p "$base/images" "$base/checks/cargo/bin" "$base/checks/rustup/toolchains"
for spec in 'LayrRunner:Case-sensitive APFS:8g' 'LayrRunnerCase:APFS:4g'; do
    IFS=: read -r volume format size <<< "$spec"
    image="$base/images/$volume.sparseimage"
    if [ ! -f "$image" ]; then
        hdiutil create -size "$size" -fs "$format" -volname "$volume" -type SPARSE -o "$image"
    fi
    if [ ! -d "/Volumes/$volume" ]; then hdiutil attach "$image" -nobrowse; fi
done
# Fetch locked dependencies first. The seed includes tools and cache only, never credentials.
cargo fetch --locked
toolchain=$(rustup show active-toolchain | awk '{print $1}')
source_rustup=${RUSTUP_HOME:-$HOME/.rustup}
source_cargo=${CARGO_HOME:-$HOME/.cargo}
cp -cR "$source_rustup/toolchains/$toolchain" "$base/checks/rustup/toolchains/"
printf '%s\n' "$toolchain" > "$base/checks/toolchain"
xcrun --find clang > "$base/checks/clang"
xcrun --show-sdk-path > "$base/checks/sdk"
# /usr/bin/ar is an xcrun shim that writes outside the sandbox. Use the real tool.
ln -sf "$(xcrun --find ar)" "$base/checks/cargo/bin/ar"
cp -cR "$source_cargo/registry" "$base/checks/cargo/"
# Resolve and unpack every locked dependency before the sandbox makes the
# registry read-only. Cargo's per-run lock files stay in the private HOME.
CARGO_HOME="$base/checks/cargo" cargo metadata --locked --offline --format-version 1 >/dev/null
echo 'Prepared dedicated APFS images and a credential-free tool seed.'
