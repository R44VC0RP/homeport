#!/bin/sh
# Builds homeport-ffi (release, arm64) and packages it for Xcode:
#   apple/Generated/HomeportFFI.xcframework   static lib + C header + modulemap
#   apple/Generated/Swift/homeport_ffi.swift  UniFFI Swift bindings
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
out="$repo/apple/Generated"
target=aarch64-apple-darwin
export MACOSX_DEPLOYMENT_TARGET=15.0

cargo build --release --manifest-path "$repo/Cargo.toml" -p homeport-ffi --lib --target "$target"
cargo build --release --manifest-path "$repo/Cargo.toml" -p homeport-ffi --features bindgen --bin uniffi-bindgen-swift

lib="$repo/target/$target/release/libhomeport_ffi.a"
dylib="$repo/target/$target/release/libhomeport_ffi.dylib"
bindgen="$repo/target/release/uniffi-bindgen-swift"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/headers" "$tmp/swift"
"$bindgen" --swift-sources "$dylib" "$tmp/swift"
"$bindgen" --headers --modulemap --module-name homeport_ffiFFI \
  --modulemap-filename module.modulemap "$dylib" "$tmp/headers"

rm -rf "$out/HomeportFFI.xcframework" "$out/Swift"
mkdir -p "$out/Swift"
cp "$tmp/swift/"*.swift "$out/Swift/"
xcodebuild -create-xcframework -library "$lib" -headers "$tmp/headers" \
  -output "$out/HomeportFFI.xcframework" >/dev/null
echo "built $out/HomeportFFI.xcframework and $out/Swift"
