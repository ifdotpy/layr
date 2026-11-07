#!/bin/zsh
# Unsigned Release build; prints diagnostics only.
D=${0:A:h}
cd $D && xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release -derivedDataPath $D/build CODE_SIGNING_ALLOWED=NO build "$@" > $D/build-unsigned.log 2>&1
rc=$?
grep -E "^/.*(error|warning):" $D/build-unsigned.log | sed "s|$D/||" | sort -u
echo "exit=$rc"; tail -2 $D/build-unsigned.log
