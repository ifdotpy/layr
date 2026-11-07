#!/bin/zsh
# Rebuild signed, reinstall to ~/Applications, drop the build-dir registration.
cd ${0:A:h}
pkill -x LayrFS 2>/dev/null
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release -derivedDataPath build-signed DEVELOPMENT_TEAM=5Q4QCM2MA2 -allowProvisioningUpdates -allowProvisioningDeviceRegistration build > build-signed.log 2>&1 || { grep error: build-signed.log; exit 1; }
LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
$LSREG -u build-signed/Build/Products/Release/LayrFSHost.app 2>/dev/null
rm -rf ~/Applications/LayrFSHost.app && ditto build-signed/Build/Products/Release/LayrFSHost.app ~/Applications/LayrFSHost.app
$LSREG -f ~/Applications/LayrFSHost.app
pluginkit -m -v -p com.apple.fskit.fsmodule | grep layr
