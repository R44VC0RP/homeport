#!/bin/sh
# Installs Homeport.app on a client Mac from a drop folder made by
# scripts/make-drop.sh: copies the app to /Applications, writes the server's
# certificate fingerprint, enables the File Provider extension and launches.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
support="$HOME/Library/Application Support/Homeport/client"

mkdir -p "$support"
cp "$here/server.sha256" "$support/server.sha256"

osascript -e 'tell application id "dev.vogel.homeport" to quit' >/dev/null 2>&1 || true
pkill -x Homeport 2>/dev/null || true
rm -rf /Applications/Homeport.app
ditto "$here/Homeport.app" /Applications/Homeport.app
spctl --assess --type execute -v /Applications/Homeport.app
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/Homeport.app
pluginkit -e use -i dev.vogel.homeport.HomeportFP || true
open /Applications/Homeport.app
echo "installed. If Finder shows no Homeport item, turn it on in System Settings > General > Login Items & Extensions > File Providers."
