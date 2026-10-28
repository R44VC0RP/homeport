#!/bin/sh
# Installs or upgrades the Homeport server (homeportd) on Linux and runs it as
# a systemd user service.
#
#   curl -fsSL https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-linux.sh | sh
#   curl -fsSL .../install-linux.sh | sh -s -- --share ~/Projects --share /srv/media --name media
#
# Options:
#   --share PATH     share a folder (repeatable); --name NAME right after it
#                    sets the name clients see (default: the folder's name)
#   --version TAG    install this release (e.g. v0.6.0; default: the latest)
#   --binary PATH    install this homeportd binary instead of downloading
#   --build          build from this source checkout (cargo build --release)
#   --enable-linger  run `sudo loginctl enable-linger` so the server starts at
#                    boot, not only at login
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
bin_dir="${HOMEPORT_BIN_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
version=latest
binary=""
build=0
linger=0
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
    --build) build=1; shift ;;
    --enable-linger) linger=1; shift ;;
    --no-service) service=0; shift ;;
    --) shift; break ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[ -n "$pending" ] && printf '%s\t\n' "$pending" >> "$shares"

[ "$(uname -s)" = Linux ] || die "this installs the Linux server; see https://github.com/$repo_slug#install"

case "$(uname -m)" in
  x86_64|amd64) target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) target=aarch64-unknown-linux-musl ;;
  *) die "no prebuilt homeportd for $(uname -m); build from source (https://github.com/$repo_slug#building-from-source)" ;;
esac

fetch() { # url dest
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "curl or wget is required"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# 1. Get the binaries.
cli=""
if [ "$build" = 1 ]; then
  repo=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd) || true
  [ -f "$repo/Cargo.toml" ] || die "--build must run from a source checkout"
  cargo build --release --locked --manifest-path "$repo/Cargo.toml" -p homeportd -p homeport-cli
  binary="$repo/target/release/homeportd"
  cli="$repo/target/release/homeport"
elif [ -z "$binary" ]; then
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
    fetch "$url" "$tmp/$asset"
    if [ -z "${HOMEPORT_URL:-}" ]; then
      fetch "${url%/*}/SHA256SUMS" "$tmp/SHA256SUMS"
      want=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
      [ -n "$want" ] || die "$asset is not listed in SHA256SUMS"
      [ "$(sha256 "$tmp/$asset")" = "$want" ] || die "checksum mismatch for $asset"
    fi
  fi
  tar -xzf "$tmp/$asset" -C "$tmp"
  binary="$tmp/homeport-$target/homeportd"
  cli="$tmp/homeport-$target/homeport"
fi
[ -f "$binary" ] || die "no homeportd binary at $binary"

# 2. Install them. Copy to a temp name and rename, so a running server keeps
# its (old) file until it restarts.
install_bin() { # src name
  cp "$1" "$bin_dir/$2.new"
  chmod 755 "$bin_dir/$2.new"
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

# 3. Shares. Only ever added: an existing config is never rewritten otherwise.
is_shared() { # path
  real=$(cd "$1" 2>/dev/null && pwd -P) || return 1
  # Lines are "<name padded> <path>"; names may contain spaces, so match the end.
  "$hpd" share list 2>/dev/null | awk -v p=" $real" 'NR>1 && substr($0, length($0) - length(p) + 1) == p { found = 1 }
    END { exit !found }'
}
add_share() { # path [name]
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

if ! has_shares && [ -r /dev/tty ] && [ -w /dev/tty ] && (: < /dev/tty) 2>/dev/null; then
  printf 'Folder to share with your Mac (empty to skip): ' > /dev/tty
  read -r answer < /dev/tty || answer=""
  case "$answer" in "~"*) answer="$HOME${answer#\~}" ;; esac
  [ -n "$answer" ] && add_share "$answer"
fi

if ! has_shares; then
  say
  say "No folders are shared yet. Share one and start the server:"
  say "  $hpd share add <folder>"
  say "  $hpd service install"
  exit 0
fi

# 4. Checks that matter on Linux.
command -v tailscale >/dev/null 2>&1 ||
  warn "the tailscale CLI was not found; install Tailscale (https://tailscale.com/download/linux) and run 'sudo tailscale up'"
watches=$(cat /proc/sys/fs/inotify/max_user_watches 2>/dev/null || echo 0)
if [ "$watches" -gt 0 ] && [ "$watches" -lt 524288 ]; then
  say "tip: fs.inotify.max_user_watches is $watches; large shares need more watches:"
  say "  echo fs.inotify.max_user_watches=1048576 | sudo tee /etc/sysctl.d/90-homeport-inotify.conf && sudo sysctl --system"
fi

# 5. The service.
# The user manager's bus, for sessions without XDG_RUNTIME_DIR (su, some ssh
# setups); it exists while the user is logged in or lingering.
user_bus() {
  [ -n "${XDG_RUNTIME_DIR:-}" ] && return 0
  [ -S "/run/user/$(id -u)/bus" ] || return 1
  XDG_RUNTIME_DIR="/run/user/$(id -u)"
  export XDG_RUNTIME_DIR
}
user=$(id -un)
if [ "$service" = 0 ]; then
  say "skipped the service (--no-service); start it with: $hpd service install"
elif ! command -v systemctl >/dev/null 2>&1; then
  warn "systemd was not found; run '$hpd' under your own supervisor"
else
  if [ "$linger" = 1 ] && [ "$(loginctl show-user "$user" -p Linger --value 2>/dev/null)" != yes ]; then
    sudo loginctl enable-linger "$user" || warn "could not enable lingering"
    i=0
    while ! user_bus && [ $i -lt 20 ]; do sleep 0.25; i=$((i + 1)); done
  fi
  user_bus || true
  if ! systemctl --user show-environment >/dev/null 2>&1; then
    warn "no systemd user manager in this session; log in normally (not with su) or pass --enable-linger, then run: $hpd service install"
  else
    unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/homeportd.service"
    if [ -f "$unit" ] && [ $# -eq 0 ] && grep -q "^ExecStart=$hpd" "$unit"; then
      # An upgrade: keep the unit (and any arguments it was installed with).
      systemctl --user restart homeportd.service
      say "restarted homeportd.service"
    elif [ $# -gt 0 ]; then
      "$hpd" service install -- "$@"
    else
      "$hpd" service install
    fi
    state="${XDG_STATE_HOME:-$HOME/.local/state}/homeport/server"
    i=0
    while [ ! -f "$state/cert.sha256" ] && [ $i -lt 20 ]; do sleep 0.5; i=$((i + 1)); done
    say
    "$hpd" service status || true
  fi
fi

# 6. Pairing.
say
if "$hpd" pair --help >/dev/null 2>&1; then
  say "Next: run '$hpd pair' and open the link it prints on your Mac (or enter its 6-digit code in Homeport)."
else
  say "Next: add this server in the Homeport app on your Mac."
  state="${XDG_STATE_HOME:-$HOME/.local/state}/homeport/server"
  ip=$(tailscale ip -4 2>/dev/null | head -1 || true)
  [ -n "$ip" ] && say "  address:     $ip:47820"
  [ -f "$state/cert.sha256" ] && say "  fingerprint: $(cat "$state/cert.sha256")"
fi
exit 0
