#!/bin/zsh
# A release of Layr.app (the LayrFS app) for other Macs: Developer ID signature, notarization, stapled
# ticket, and a zip (Layr-<version>.zip in build-release/).
#
# Needs a paid Apple Developer team, signed in to Xcode (Settings > Accounts) with the role
# Account Holder or Admin. Xcode then signs with a cloud-managed Developer ID certificate and
# makes the profiles (-allowProvisioningUpdates); the App IDs dev.layr.fshost and
# dev.layr.fshost.fs need the FSKit capability (com.apple.developer.fskit.fsmodule).
# Notarization: by default Xcode uploads the app with the signed-in account. With
# NOTARY_PROFILE (xcrun notarytool store-credentials <name> ...), notarytool does it instead.
#
# Usage: TEAM_ID=<team> [NOTARY_PROFILE=<name>] [VERSION=<x.y>] ./release.sh
set -euo pipefail
D=${0:A:h}
cd $D
: ${TEAM_ID:?set TEAM_ID to the Apple Developer team}
NOTARY_PROFILE=${NOTARY_PROFILE:-}
VERSION=${VERSION:-$(awk '/MARKETING_VERSION/ { gsub(/"/, "", $2); print $2; exit }' project.yml)}
OUT=$D/build-release
rm -rf $OUT
mkdir -p $OUT

if [ -n "$NOTARY_PROFILE" ]; then
    xcrun notarytool history --keychain-profile $NOTARY_PROFILE >/dev/null 2>&1 ||
        { echo "no notary credentials named $NOTARY_PROFILE (see the header of this script)" >&2; exit 1; }
fi
DESTINATION=export
[ -n "$NOTARY_PROFILE" ] || DESTINATION=upload

command -v xcodegen >/dev/null && xcodegen generate >/dev/null
echo "archive"
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release \
    -archivePath $OUT/LayrFSHost.xcarchive DEVELOPMENT_TEAM=$TEAM_ID MARKETING_VERSION=$VERSION \
    -allowProvisioningUpdates archive > $OUT/archive.log 2>&1 ||
    { grep -E "error:" $OUT/archive.log | sort -u >&2; exit 1; }

cat > $OUT/export.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>method</key><string>developer-id</string>
    <key>teamID</key><string>$TEAM_ID</string>
    <key>signingStyle</key><string>automatic</string>
    <key>destination</key><string>$DESTINATION</string>
</dict>
</plist>
EOF
echo "export (Developer ID, $DESTINATION)"
xcodebuild -exportArchive -archivePath $OUT/LayrFSHost.xcarchive -exportOptionsPlist $OUT/export.plist \
    -exportPath $OUT/export -allowProvisioningUpdates > $OUT/export.log 2>&1 ||
    { grep -E "error:" $OUT/export.log | sort -u >&2; exit 1; }
APP=$OUT/export/Layr.app
ZIP=$OUT/Layr-$VERSION.zip

if [ "$DESTINATION" = upload ]; then
    # Xcode uploaded the archive for notarization: wait for the ticket, then take the app.
    echo "notarize (Xcode account)"
    for _ in $(seq 1 120); do
        xcodebuild -exportNotarizedApp -archivePath $OUT/LayrFSHost.xcarchive -exportPath $OUT/export \
            > $OUT/notarized.log 2>&1 && break
        sleep 15
    done
    [ -d $APP ] || { tail -20 $OUT/notarized.log >&2; exit 1; }
else
    echo "notarize (notarytool)"
    ditto -c -k --keepParent $APP $OUT/notarize.zip
    xcrun notarytool submit $OUT/notarize.zip --keychain-profile $NOTARY_PROFILE --wait > $OUT/notarize.log 2>&1 ||
        { cat $OUT/notarize.log >&2; exit 1; }
    grep -q "status: Accepted" $OUT/notarize.log || { cat $OUT/notarize.log >&2; exit 1; }
    rm $OUT/notarize.zip
    xcrun stapler staple $APP
fi

codesign --verify --deep --strict $APP
# The output first, then the test: with pipefail, grep -q ends the pipe early and codesign fails on the closed
# pipe, so a signed app counted as unsigned (2026-11-22).
signature=$(codesign -dvv $APP 2>&1)
[[ "$signature" == *"Authority=Developer ID Application"* ]] ||
    { echo "the app is not signed with Developer ID" >&2; exit 1; }
entitlements=$(codesign -d --entitlements - $APP/Contents/Extensions/LayrFS.appex 2>/dev/null) || true
[[ "$entitlements" == *com.apple.developer.fskit.fsmodule* ]] ||
    { echo "the extension lacks the FSKit entitlement" >&2; exit 1; }
xcrun stapler validate $APP
spctl --assess --type execute --verbose $APP
ditto -c -k --keepParent $APP $ZIP
shasum -a 256 $ZIP | tee $ZIP.sha256
echo "done: $ZIP"
