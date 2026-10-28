#!/bin/sh
# Installs or upgrades the Homeport server (homeportd) on macOS and runs it as
# a LaunchAgent. (Homebrew users: see packaging/homebrew/homeportd.rb.)
#
#   curl -fsSL https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-macos.sh | sh
#   curl -fsSL .../install-macos.sh | sh -s -- --share ~/Projects --share ~/Movies --name movies
#
# Options:
#   --share PATH     share a folder (repeatable); --name NAME right after it
#                    sets the name clients see (default: the folder's name)
#   --version TAG    install this release (e.g. v0.6.0; default: the latest)
#   --binary PATH    install this homeportd binary instead of downloading
#   --no-service     install the binaries and shares only
#   -- ARGS          extra homeportd arguments for the service (e.g. --port)
#
# Environment: HOMEPORT_URL (archive URL or local path to install instead of
# the GitHub release), HOMEPORT_BIN_DIR (default ~/.local/bin).
#
# Safe to re-run: binaries are replaced in place, the existing config and
# shares are kept (shares are only ever added), and a running service is
# restarted with its existing arguments.
set -eu

repo_slug=R44VC0RP/homeport
bin_dir="${HOMEPORT_BIN_DIR:-$HOME/.local/bin}"
config="$HOME/Library/Application Support/Homeport/server.toml"
plist="$HOME/Library/LaunchAgents/dev.vogel.homeportd.plist"
state="$HOME/Library/Application Support/Homeport/server"
version=latest
binary=""
service=1
pending=""
tab=$(printf '\t')

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
shares="$tmp/shares"
: > "$shares"

while [ $# -gt 0 ]; do
  case "$1" in
    --share) [ $# -ge 2 ] || die "--share needs a path"
             [ -n "$pending" ] && printf '%s\t\n' "$pending" >> "$shares"
             pending=$2; shift 2 ;;
    --name) [ $# -ge 2 ] || die "--name needs a value"
            [ -n "$pending" ] || die "--name must follow a --share"
            printf '%s\t%s\n' "$pending" "$2" >> "$shares"; pending=""; shift 2 ;;
    --version) [ $# -ge 2 ] || die "--version needs a tag"; version=$2; shift 2 ;;
    --binary) [ $# -ge 2 ] || die "--binary needs a path"; binary=$2; shift 2 ;;
    --no-service) service=0; shift ;;
    --) shift; break ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[ -n "$pending" ] && printf '%s\t\n' "$pending" >> "$shares"

[ "$(uname -s)" = Darwin ] || die "this installs the macOS server; see https://github.com/$repo_slug#install"
case "$(uname -m)" in
  arm64) target=aarch64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin ;;
  *) die "no prebuilt homeportd for $(uname -m)" ;;
esac

# 1. Get the binaries.
cli=""
if [ -z "$binary" ]; then
  asset="homeport-$target.tar.gz"
  if [ -n "${HOMEPORT_URL:-}" ]; then
    url=$HOMEPORT_URL
  elif [ "$version" = latest ]; then
    url="https://github.com/$repo_slug/releases/latest/download/$asset"
  else
    url="https://github.com/$repo_slug/releases/download/$version/$asset"
  fi
  say "downloading $url"
  if [ -f "$url" ]; then
    cp "$url" "$tmp/$asset"
  else
    curl -fsSL --retry 3 -o "$tmp/$asset" "$url"
    if [ -z "${HOMEPORT_URL:-}" ]; then
      curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "${url%/*}/SHA256SUMS"
      want=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
      [ -n "$want" ] || die "$asset is not listed in SHA256SUMS"
      [ "$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)" = "$want" ] || die "checksum mismatch for $asset"
    fi
  fi
  tar -xzf "$tmp/$asset" -C "$tmp"
  binary="$tmp/homeport-$target/homeportd"
  cli="$tmp/homeport-$target/homeport"
fi
[ -f "$binary" ] || die "no homeportd binary at $binary"

# 2. Install them (temp name + rename: a running server keeps its file).
install_bin() { # src name
  cp "$1" "$bin_dir/$2.new"
  chmod 755 "$bin_dir/$2.new"
  xattr -d com.apple.quarantine "$bin_dir/$2.new" 2>/dev/null || true
  mv -f "$bin_dir/$2.new" "$bin_dir/$2"
}
mkdir -p "$bin_dir"
install_bin "$binary" homeportd
[ -n "$cli" ] && [ -f "$cli" ] && install_bin "$cli" homeport
hpd="$bin_dir/homeportd"
say "installed $("$hpd" --version) in $bin_dir"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) warn "$bin_dir is not on your PATH; add it in your shell profile to run homeportd directly" ;;
esac

# 3. Shares. Only ever added; an existing config is never rewritten otherwise.
# A new config starts with an explicit empty list.
start_config() {
  if [ ! -e "$config" ]; then
    mkdir -p "$(dirname "$config")"
    printf '# homeportd server configuration\n\nshares = []\n' > "$config"
  fi
}
is_shared() { # path
  real=$(cd "$1" 2>/dev/null && pwd -P) || return 1
  # Lines are "<name padded> <path>"; names may contain spaces, so match the end.
  "$hpd" share list 2>/dev/null | awk -v p=" $real" 'NR>1 && substr($0, length($0) - length(p) + 1) == p { found = 1 }
    END { exit !found }'
}
add_share() { # path [name]
  start_config
  if is_shared "$1"; then
    say "already shared: $1"
  elif [ -n "${2:-}" ]; then
    "$hpd" share add "$1" --name "$2"
  else
    "$hpd" share add "$1"
  fi
}
has_shares() {
  "$hpd" share list 2>/dev/null | awk 'NR>1' | grep -q '/'
}
while IFS="$tab" read -r path name; do
  [ -n "$path" ] && add_share "$path" "$name"
done < "$shares"

if { [ ! -e "$config" ] || ! has_shares; } && [ -r /dev/tty ] && (: < /dev/tty) 2>/dev/null; then
  printf 'Folder to share with your other Mac (empty to skip): ' > /dev/tty
  read -r answer < /dev/tty || answer=""
  case "$answer" in "~"*) answer="$HOME${answer#\~}" ;; esac
  [ -n "$answer" ] && add_share "$answer"
fi

if [ ! -e "$config" ]; then
  say
  say "No folders are shared yet. Share the folders you want, then start the server:"
  say "  $hpd share add <folder>"
  say "  $hpd service install"
  exit 0
fi
if ! has_shares; then
  say
  say "No folders are shared yet. Share one and start the server:"
  say "  $hpd share add <folder>"
  say "  $hpd service install"
  exit 0
fi

[ -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ] || command -v tailscale >/dev/null 2>&1 ||
  warn "Tailscale was not found; install it (https://tailscale.com/download/mac) and log in"

# 4. The LaunchAgent.
uid=$(id -u)
if [ "$service" = 0 ]; then
  say "skipped the service (--no-service); start it with: $hpd service install"
elif [ -f "$plist" ] && [ $# -eq 0 ] && grep -q "<string>$hpd</string>" "$plist"; then
  # An upgrade: keep the plist (and any arguments it was installed with).
  launchctl kickstart -k "gui/$uid/dev.vogel.homeportd" >/dev/null 2>&1 ||
    launchctl bootstrap "gui/$uid" "$plist"
  say "restarted dev.vogel.homeportd"
elif [ $# -gt 0 ]; then
  "$hpd" service install -- "$@"
else
  "$hpd" service install
fi
if [ "$service" = 1 ]; then
  i=0
  while [ ! -f "$state/cert.sha256" ] && [ $i -lt 20 ]; do sleep 0.5; i=$((i + 1)); done
  say
  "$hpd" service status || true
  say
  say "If you share ~/Downloads, ~/Documents or ~/Desktop, macOS asks whether homeportd may"
  say "access them: click Allow (System Settings > Privacy & Security > Files and Folders)."
fi

# 5. Pairing.
say
if "$hpd" pair --help >/dev/null 2>&1; then
  say "Next: run '$hpd pair' and open the link it prints on your Mac (or enter its 6-digit code in Homeport)."
else
  say "Next: add this server in the Homeport app on your Mac."
  ts=/Applications/Tailscale.app/Contents/MacOS/Tailscale
  [ -x "$ts" ] || ts=tailscale
  ip=$("$ts" ip -4 2>/dev/null | head -1 || true)
  [ -n "$ip" ] && say "  address:     $ip:47820"
  [ -f "$state/cert.sha256" ] && say "  fingerprint: $(cat "$state/cert.sha256")"
fi
exit 0
