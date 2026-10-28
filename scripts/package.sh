#!/bin/sh
# Builds homeportd and the homeport CLI for one target and packs them, with
# LICENSE and packaging/README.txt, into dist/homeport-<target>.tar.gz
# (.zip for Windows). Used by the release workflow and CI.
#
#   scripts/package.sh <target> [--zig]
#
#   --zig   build with cargo-zigbuild (static musl Linux binaries from any host)
#
# Release binaries are stripped of debug info (the workspace profile keeps
# line tables for local profiling). Windows binaries link the C runtime
# statically, so they need no Visual C++ redistributable.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?usage: scripts/package.sh <target> [--zig]}
build=build
[ "${2:-}" = --zig ] && build=zigbuild

export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=symbols
case "$target" in
  *-windows-msvc) export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" ;;
  *-apple-darwin) export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}" ;;
esac

cd "$repo"
cargo "$build" --release --locked -p homeportd -p homeport-cli --target "$target"

name="homeport-$target"
stage="$repo/dist/$name"
rm -rf "$stage"
mkdir -p "$stage"
case "$target" in
  *-windows-*) exe=.exe ;;
  *) exe= ;;
esac
cp "target/$target/release/homeportd$exe" "target/$target/release/homeport$exe" "$stage/"
cp LICENSE "$stage/"
cp packaging/README.txt "$stage/"

cd "$repo/dist"
case "$target" in
  *-windows-*)
    rm -f "$name.zip"
    7z a -tzip "$name.zip" "$name" >/dev/null
    echo "dist/$name.zip"
    ;;
  *)
    rm -f "$name.tar.gz"
    COPYFILE_DISABLE=1 tar -czf "$name.tar.gz" "$name"
    echo "dist/$name.tar.gz"
    ;;
esac
