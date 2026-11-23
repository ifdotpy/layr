#!/bin/zsh
# The macOS installer of layr: layr-<version>.pkg in packaging/macos/build/. It installs
#   /usr/local/bin/layr          the command (Developer ID signature, hardened runtime)
#   /Applications/LayrFSHost.app the LayrFS file system extension of fast lines
# and then opens the app as the person who installs it: the system registers the extension, and
# the app opens the settings pane where that person turns LayrFS on (only a person can).
# macOS 27 or later, Apple silicon.
#
# Needs, in the login keychain of this Mac (Xcode > Settings > Accounts > Manage Certificates,
# role Account Holder or Admin):
#   Developer ID Application: <name> (<team>)   signs layr
#   Developer ID Installer: <name> (<team>)     signs the package
# and notarytool credentials (once): xcrun notarytool store-credentials <profile> --apple-id <id>
#   --team-id <team> (an app-specific password from account.apple.com).
#
# Usage: TEAM_ID=<team> NOTARY_PROFILE=<profile> ./build-pkg.sh
#   UNSIGNED=1 ./build-pkg.sh   a package without signatures or notarization (a test of the layout)
#   LAYR_BIN=<path>              package this release build of layr (its version must be the one of
#                                Cargo.toml) instead of a cargo build in target/
set -euo pipefail
D=${0:A:h}
REPO=${D:h:h}
OUT=$D/build
UNSIGNED=${UNSIGNED:-}
LAYR_BIN=${LAYR_BIN:-}
VERSION=$(awk -F'"' '/^version *=/ { print $2; exit }' $REPO/Cargo.toml)
PKG=$OUT/layr-$VERSION.pkg
[ "$(uname -m)" = arm64 ] || { echo "build on Apple silicon" >&2; exit 1; }

if [ -z "$UNSIGNED" ]; then
    : ${TEAM_ID:?set TEAM_ID to the Apple Developer team}
    : ${NOTARY_PROFILE:?set NOTARY_PROFILE (see the header of this script)}
    identity() { # identity <kind>: the first valid identity of that kind for the team
        local args=(-v)
        [ -n "${2:-}" ] && args+=(-p "$2")
        security find-identity "${args[@]}" | awk -v k="$1" -v t="($TEAM_ID)" \
            'index($0, k) && index($0, t) { sub(/^[^"]*"/, ""); sub(/"$/, ""); print; exit }'
    }
    APP_ID=$(identity "Developer ID Application:" codesigning)
    INSTALLER_ID=$(identity "Developer ID Installer:")
    [ -n "$APP_ID" ] || { echo "no Developer ID Application certificate of team $TEAM_ID (see the header)" >&2; exit 1; }
    [ -n "$INSTALLER_ID" ] || { echo "no Developer ID Installer certificate of team $TEAM_ID (see the header)" >&2; exit 1; }
    xcrun notarytool history --keychain-profile $NOTARY_PROFILE >/dev/null 2>&1 ||
        { echo "no notarytool credentials named $NOTARY_PROFILE (see the header)" >&2; exit 1; }
fi
rm -rf $OUT
mkdir -p $OUT/root/usr/local/bin $OUT/root/Applications $OUT/scripts

echo "layr $VERSION"
if [ -z "$LAYR_BIN" ]; then
    (cd $REPO && cargo build --release --locked -q)
    LAYR_BIN=$REPO/target/release/layr
fi
[[ "$("$LAYR_BIN" version)" == "layr version $VERSION "* ]] ||
    { echo "$LAYR_BIN is not layr $VERSION" >&2; exit 1; }
cp "$LAYR_BIN" $OUT/root/usr/local/bin/layr
strip -S $OUT/root/usr/local/bin/layr
if [ -z "$UNSIGNED" ]; then
    codesign --force --options runtime --timestamp --sign "$APP_ID" $OUT/root/usr/local/bin/layr
fi

echo "LayrFSHost.app"
if [ -z "$UNSIGNED" ]; then
    # Developer ID signature and notarization of the app (tools/layrfs/release.sh).
    TEAM_ID=$TEAM_ID NOTARY_PROFILE=$NOTARY_PROFILE $REPO/tools/layrfs/release.sh >/dev/null
    ditto $REPO/tools/layrfs/build-release/export/LayrFSHost.app $OUT/root/Applications/LayrFSHost.app
else
    (cd $REPO/tools/layrfs && { command -v xcodegen >/dev/null && xcodegen generate >/dev/null || true; } &&
        xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release \
            -derivedDataPath $OUT/xcode CODE_SIGNING_ALLOWED=NO build > $OUT/xcode.log 2>&1) ||
        { grep -E "error:" $OUT/xcode.log | sort -u >&2; exit 1; }
    ditto $OUT/xcode/Build/Products/Release/LayrFSHost.app $OUT/root/Applications/LayrFSHost.app
fi

cat > $OUT/scripts/postinstall <<'EOF'
#!/bin/sh
# Open the app as the person at the screen: the system registers the extension, and the app
# opens the settings pane when LayrFS is off. No one at the screen: the first open does it.
# Another target volume ($3) is not this system: its first start registers the app.
[ "${3:-/}" = / ] || exit 0
user=$(stat -f %Su /dev/console)
[ -n "$user" ] && [ "$user" != root ] || exit 0
uid=$(id -u "$user")
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/LayrFSHost.app
launchctl asuser "$uid" sudo -u "$user" /usr/bin/open -g /Applications/LayrFSHost.app || true
exit 0
EOF
chmod 755 $OUT/scripts/postinstall

# No quarantine or other attributes in the payload (the system keeps com.apple.provenance; the
# installer sets it back as an attribute).
xattr -cr $OUT/root
# The app stays in /Applications: without this the installer moves an older copy that it finds
# elsewhere (BundleIsRelocatable), and a moved app turns the extension off.
pkgbuild --analyze --root $OUT/root $OUT/components.plist >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO $OUT/components.plist
pkgbuild --root $OUT/root --component-plist $OUT/components.plist --scripts $OUT/scripts \
    --identifier dev.layr.pkg --version $VERSION --install-location / $OUT/layr-component.pkg >/dev/null

cat > $OUT/distribution.xml <<EOF
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>layr $VERSION</title>
    <options customize="never" require-scripts="true" hostArchitectures="arm64"/>
    <domains enable_localSystem="true"/>
    <allowed-os-versions><os-version min="27.0"/></allowed-os-versions>
    <choices-outline><line choice="default"><line choice="dev.layr.pkg"/></line></choices-outline>
    <choice id="default"/>
    <choice id="dev.layr.pkg" visible="false"><pkg-ref id="dev.layr.pkg"/></choice>
    <pkg-ref id="dev.layr.pkg" version="$VERSION" onConclusion="none">layr-component.pkg</pkg-ref>
</installer-gui-script>
EOF
if [ -n "$UNSIGNED" ]; then
    productbuild --distribution $OUT/distribution.xml --package-path $OUT $PKG >/dev/null
else
    productbuild --distribution $OUT/distribution.xml --package-path $OUT --sign "$INSTALLER_ID" --timestamp $PKG >/dev/null
    echo "notarize"
    xcrun notarytool submit $PKG --keychain-profile $NOTARY_PROFILE --wait > $OUT/notarize.log 2>&1 ||
        { cat $OUT/notarize.log >&2; exit 1; }
    grep -q "status: Accepted" $OUT/notarize.log || { cat $OUT/notarize.log >&2; exit 1; }
    xcrun stapler staple $PKG >/dev/null
    xcrun stapler validate $PKG >/dev/null
    spctl --assess --type install --verbose $PKG
fi
shasum -a 256 $PKG | tee $PKG.sha256
echo "done: $PKG"
