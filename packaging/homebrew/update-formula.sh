#!/bin/sh
# Sets packaging/homebrew/homeportd.rb's version and checksums from a GitHub
# Release's SHA256SUMS.
#
#   packaging/homebrew/update-formula.sh v0.6.0
set -eu

tag=${1:?usage: update-formula.sh <tag>}
formula="$(cd "$(dirname "$0")" && pwd)/homeportd.rb"
tmp=$(mktemp)
sums=$(mktemp)
trap 'rm -f "$tmp" "$sums"' EXIT
gh release download "$tag" --repo R44VC0RP/homeport --pattern SHA256SUMS --output - > "$sums"

# Each sha256 line follows the url line naming its archive.
awk -v version="${tag#v}" -v sums="$sums" '
  BEGIN { while ((getline line < sums) > 0) { split(line, f, " +"); if (f[2] != "") sum[f[2]] = f[1] } }
  /^  version "/ { sub(/"[^"]*"/, "\"" version "\"") }
  /url "/ { match($0, /homeport-[^"\/]*\.tar\.gz/); asset = substr($0, RSTART, RLENGTH) }
  /sha256 "/ && asset != "" {
    if (!(asset in sum)) { print "no checksum for " asset > "/dev/stderr"; exit 1 }
    sub(/"[0-9a-f]*"/, "\"" sum[asset] "\""); asset = ""
  }
  { print }
' "$formula" > "$tmp"
cp "$tmp" "$formula"
echo "updated $formula to ${tag#v}"
