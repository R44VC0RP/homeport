#!/bin/sh
# Builds homeport-ffi and Homeport.app (with the HomeportFP File Provider
# extension), installs it to /Applications and launches it.
#
#   scripts/build-app.sh            build, install, launch
#   scripts/build-app.sh --no-ffi   skip the Rust build (Swift-only changes)
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
dest=/Applications/Homeport.app

if [ "${1:-}" != "--no-ffi" ]; then
  "$repo/scripts/build-ffi.sh"
fi

cd "$repo/apple"
/opt/homebrew/bin/xcodegen generate --quiet
log="$repo/build/xcodebuild.log"
mkdir -p "$repo/build"
if ! xcodebuild -project Homeport.xcodeproj -scheme Homeport -configuration Release \
  -derivedDataPath "$repo/build/DerivedData" build -quiet >"$log" 2>&1; then
  grep -e "error:" -e "BUILD FAILED" "$log" >&2 || tail -20 "$log" >&2
  exit 1
fi
grep -v -e "appintentsmetadataprocessor" -e "^ld: warning: object file" "$log" || true
app="$repo/build/DerivedData/Build/Products/Release/Homeport.app"
codesign --verify --deep --strict "$app"

# Replace the installed copy. The extension process is restarted by the
# system when its bundle changes.
osascript -e 'tell application id "dev.vogel.homeport" to quit' >/dev/null 2>&1 || true
pkill -x Homeport 2>/dev/null || true
pkill -x HomeportFP 2>/dev/null || true
rm -rf "$dest"
ditto "$app" "$dest"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$dest"
open "$dest"
echo "installed and launched $dest"
