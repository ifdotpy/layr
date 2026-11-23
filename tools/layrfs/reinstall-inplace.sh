#!/bin/zsh
# Rebuild signed and copy over the installed app without unregistering it. The copy keeps the path
# that the system registered (an app up to 0.53 is LayrFSHost.app): a moved app turns LayrFS off.
cd ${0:A:h}
xcodebuild -project LayrFSHost.xcodeproj -scheme LayrFSHost -configuration Release -derivedDataPath build-signed DEVELOPMENT_TEAM=5Q4QCM2MA2 -allowProvisioningUpdates -allowProvisioningDeviceRegistration build > build-signed.log 2>&1 || { grep error: build-signed.log; exit 1; }
appex=$(pluginkit -m -v -i dev.layr.fshost.fs | awk -F'\t' 'NF { print $NF; exit }' | sed 's/^ *//')
dest=${appex:+${appex:h:h:h}}
dest=${dest:-~/Applications/Layr.app}
pkill -x LayrFS 2>/dev/null; sleep 1
ditto build-signed/Build/Products/Release/Layr.app $dest
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u build-signed/Build/Products/Release/Layr.app 2>/dev/null
sleep 2
echo "installed: $dest"
pluginkit -m -A -D -v -i dev.layr.fshost.fs
