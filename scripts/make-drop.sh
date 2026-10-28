#!/bin/sh
# Builds, notarizes and staples Homeport.app, packs it with the server's
# certificate fingerprint and install.sh into a zip, and Taildrops it.
#
#   scripts/make-drop.sh [--no-ffi] [--no-send] [host]   (host: airbook)
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
ffi=""
send=1
host=airbook
for a in "$@"; do
  case "$a" in
    --no-ffi) ffi=--no-ffi ;;
    --no-send) send=0 ;;
    *) host=$a ;;
  esac
done

"$repo/scripts/build-app.sh" $ffi
app="$repo/build/DerivedData/Build/Products/Release/Homeport.app"
version=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$app/Contents/Info.plist")
build=$(git -C "$repo" rev-parse --short HEAD)
name="homeport-$version-$build"
out="$repo/build/drop"
dir="$out/$name"

rm -rf "$dir" "$out/$name.zip"
mkdir -p "$dir"
ditto -c -k --norsrc --keepParent "$app" "$out/notarize.zip"
xcrun notarytool submit "$out/notarize.zip" --keychain-profile isocast-notary --wait
rm -f "$out/notarize.zip"
xcrun stapler staple "$app"
ditto --norsrc "$app" "$dir/Homeport.app"
cp "$HOME/Library/Application Support/Homeport/server/cert.sha256" "$dir/server.sha256"
cp "$repo/scripts/install-client.sh" "$dir/install.sh"
cp "$repo/scripts/hpbench.py" "$dir/hpbench.py"
chmod +x "$dir/install.sh"
(cd "$out" && COPYFILE_DISABLE=1 ditto -c -k --norsrc --keepParent "$name" "$name.zip")
if unzip -l "$out/$name.zip" | grep -q '/\._'; then
  echo "error: AppleDouble entries in $out/$name.zip" >&2
  exit 1
fi
shasum -a 256 "$out/$name.zip"
if [ "$send" = 1 ]; then
  /Applications/Tailscale.app/Contents/MacOS/Tailscale file cp "$out/$name.zip" "$host:"
  echo "sent $name.zip to $host (lands in ~/Downloads)"
fi
