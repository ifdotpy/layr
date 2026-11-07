#!/bin/zsh
# Compiles the FSKit-free backend, the item table and the overlay with test drivers and runs them on temp trees.
set -e
D=${0:A:h:h}
OUT=$D/build/selftest
mkdir -p $OUT
xcrun swiftc -O -swift-version 5 -target arm64-apple-macos27.0 -o $OUT/layrfs-selftest \
  $D/LayrFS/Posix.swift $D/LayrFS/Stats.swift $D/LayrFS/LayrItem.swift $D/LayrFS/MemIndex.swift $D/LayrFS/Overlay.swift \
  $D/selftest/main.swift
xcrun swiftc -O -swift-version 5 -target arm64-apple-macos27.0 -o $OUT/layrfs-overlay-selftest \
  $D/LayrFS/Posix.swift $D/LayrFS/Stats.swift $D/LayrFS/MemIndex.swift $D/LayrFS/Overlay.swift \
  $D/selftest/overlay/main.swift
TMPDIR_BASE=${1:-$OUT}
$OUT/layrfs-selftest $TMPDIR_BASE
$OUT/layrfs-overlay-selftest $TMPDIR_BASE
