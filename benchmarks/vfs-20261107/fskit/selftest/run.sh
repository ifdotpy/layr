#!/bin/zsh
# Compiles the FSKit-free backend plus the item table with a test driver and runs it on a temp tree.
set -e
D=${0:A:h:h}
OUT=$D/build/selftest
mkdir -p $OUT
xcrun swiftc -O -swift-version 5 -target arm64-apple-macos27.0 -o $OUT/layrfs-selftest \
  $D/LayrFS/Posix.swift $D/LayrFS/Stats.swift $D/LayrFS/LayrItem.swift $D/LayrFS/MemIndex.swift $D/selftest/main.swift
TMPDIR_BASE=${1:-$OUT}
$OUT/layrfs-selftest $TMPDIR_BASE
