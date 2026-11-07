#!/bin/bash
# RUSTC_WRAPPER: $1 is rustc. Adds path remap and optional oso_prefix.
rustc="$1"; shift
extra=(--remap-path-prefix="$LINE_DIR=/work")
if [ -n "$OSO" ]; then extra+=(-C "link-arg=-Wl,-oso_prefix,$LINE_DIR/"); fi
exec "$rustc" "$@" "${extra[@]}"
