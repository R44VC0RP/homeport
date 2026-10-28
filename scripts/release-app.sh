#!/bin/sh
# Builds the Developer ID signed, notarized and stapled Homeport.app, packs it
# into Homeport.dmg (signed, notarized, stapled) and uploads it to the GitHub
# Release for <tag>, adding its checksum to the release's SHA256SUMS.
#
#   scripts/release-app.sh <tag> [--no-ffi] [--no-upload] [--no-notarize]
#
#   --no-ffi        skip the Rust build (reuse apple/Generated)
#   --no-upload     build build/release/Homeport.dmg only
#   --no-notarize   skip notarization (a local test build; implies --no-upload)
#
# Needs the "Developer ID Application" identity in the keychain and the
# notarytool keychain profile isocast-notary (see make-drop.sh). The release
# workflow creates the draft release when the tag is pushed; run this after
# it finishes, then publish the draft.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tag=""
ffi=1
upload=1
notarize=1
for a in "$@"; do
  case "$a" in
    --no-ffi) ffi=0 ;;
    --no-upload) upload=0 ;;
    --no-notarize) notarize=0; upload=0 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    -*) echo "unknown option: $a" >&2; exit 2 ;;
    *) tag=$a ;;
  esac
done
[ -n "$tag" ] || { echo "usage: scripts/release-app.sh <tag> [--no-ffi] [--no-upload] [--no-notarize]" >&2; exit 2; }

profile=isocast-notary
identity="${HOMEPORT_SIGN_IDENTITY:-Developer ID Application: Ryan Vogel (9G68SMNHEU)}"
out="$repo/build/release"
derived="$out/DerivedData"
app="$derived/Build/Products/Release/Homeport.app"
dmg="$out/Homeport.dmg"

version=$(sed -n 's/^ *MARKETING_VERSION: "\(.*\)"/\1/p' "$repo/apple/project.yml")
if [ "v$version" != "$tag" ]; then
  echo "error: apple/project.yml MARKETING_VERSION is $version, tag is $tag" >&2
  exit 1
fi
if [ "$upload" = 1 ]; then
  gh release view "$tag" --repo R44VC0RP/homeport >/dev/null ||
    { echo "error: no GitHub release $tag yet (push the tag and let the release workflow finish)" >&2; exit 1; }
  if ! git -C "$repo" diff --quiet HEAD -- apple crates; then
    echo "error: uncommitted changes in apple/ or crates/" >&2
    exit 1
  fi
  [ "$(git -C "$repo" rev-parse HEAD)" = "$(git -C "$repo" rev-parse "$tag^{commit}")" ] ||
    { echo "error: HEAD is not $tag; check out the tag first" >&2; exit 1; }
fi

# 1. Build (the same steps as build-app.sh, without installing).
[ "$ffi" = 1 ] && "$repo/scripts/build-ffi.sh"
rm -rf "$out"
mkdir -p "$out"
(cd "$repo/apple" && xcodegen generate --quiet)
if ! xcodebuild -project "$repo/apple/Homeport.xcodeproj" -scheme Homeport -configuration Release \
  -derivedDataPath "$derived" build -quiet >"$out/xcodebuild.log" 2>&1; then
  grep -e "error:" -e "BUILD FAILED" "$out/xcodebuild.log" >&2 || tail -20 "$out/xcodebuild.log" >&2
  exit 1
fi
codesign --verify --deep --strict "$app"

notarize() { # file
  xcrun notarytool submit "$1" --keychain-profile "$profile" --wait
}

# 2. Notarize and staple the app, so it opens offline once copied out of the DMG.
if [ "$notarize" = 1 ]; then
  ditto -c -k --norsrc --keepParent "$app" "$out/notarize.zip"
  notarize "$out/notarize.zip"
  rm -f "$out/notarize.zip"
  xcrun stapler staple "$app"
fi

# 3. The DMG: the app and an Applications link to drag it onto.
stage="$out/dmg"
mkdir -p "$stage"
ditto --norsrc "$app" "$stage/Homeport.app"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname "Homeport $version" -srcfolder "$stage" -fs APFS -format ULFO -ov "$dmg"
rm -rf "$stage"
if security find-identity -v -p codesigning | grep -q "$identity"; then
  codesign --force --sign "$identity" --timestamp "$dmg"
else
  echo "warning: no '$identity' identity; the DMG is unsigned" >&2
fi
if [ "$notarize" = 1 ]; then
  notarize "$dmg"
  xcrun stapler staple "$dmg"
  spctl --assess --type open --context context:primary-signature -v "$dmg"
fi
sum=$(cd "$out" && shasum -a 256 Homeport.dmg)
echo "$sum"

if [ "$upload" = 0 ]; then
  echo "built $dmg (not uploaded)"
  exit 0
fi

# 4. Upload, and add the DMG to SHA256SUMS.
gh release download "$tag" --repo R44VC0RP/homeport --pattern SHA256SUMS --dir "$out" --clobber
grep -v ' Homeport.dmg$' "$out/SHA256SUMS" > "$out/SHA256SUMS.new" || true
echo "$sum" >> "$out/SHA256SUMS.new"
mv "$out/SHA256SUMS.new" "$out/SHA256SUMS"
gh release upload "$tag" --repo R44VC0RP/homeport --clobber "$dmg" "$out/SHA256SUMS"
echo "uploaded Homeport.dmg to $tag; publish the draft when ready:"
echo "  gh release edit $tag --repo R44VC0RP/homeport --draft=false"
