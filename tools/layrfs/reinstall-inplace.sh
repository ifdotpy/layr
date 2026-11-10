#!/bin/zsh
# Rebuild signed and copy over the installed app without unregistering it.
cd ${0:A:h}
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release -derivedDataPath build-signed DEVELOPMENT_TEAM=5Q4QCM2MA2 -allowProvisioningUpdates -allowProvisioningDeviceRegistration build > build-signed.log 2>&1 || { grep error: build-signed.log; exit 1; }
pkill -x LayrFS 2>/dev/null; sleep 1
ditto build-signed/Build/Products/Release/LayrFSHost.app ~/Applications/LayrFSHost.app
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u build-signed/Build/Products/Release/LayrFSHost.app 2>/dev/null
sleep 2
pluginkit -m -A -D -v -i dev.layr.fshost.fs
