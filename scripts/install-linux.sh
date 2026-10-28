#!/bin/sh
# Installs homeportd on Linux as a systemd user service.
#
#   scripts/install-linux.sh [--binary PATH] [--share PATH [--name NAME]]... [-- homeportd args]
#
# Without --binary it builds from this checkout (cargo build --release).
# The binary goes to ~/.local/bin/homeportd; shares are added to
# ~/.config/homeport/server.toml; `homeportd service install` writes
# ~/.config/systemd/user/homeportd.service and starts it. Lingering is turned
# on (with sudo) so the server runs from boot without a login.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
binary=""
pending_share=""

add_share() { # path [name]
  if [ -n "${2:-}" ]; then
    "$bin_dir/homeportd" share add "$1" --name "$2"
  else
    "$bin_dir/homeportd" share add "$1"
  fi
}

shares_file=$(mktemp)
trap 'rm -f "$shares_file"' EXIT
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) binary="$2"; shift 2 ;;
    --share) [ -n "$pending_share" ] && printf '%s\t\n' "$pending_share" >> "$shares_file"
             pending_share="$2"; shift 2 ;;
    --name) [ -n "$pending_share" ] || { echo "--name needs a --share before it" >&2; exit 2; }
            printf '%s\t%s\n' "$pending_share" "$2" >> "$shares_file"; pending_share=""; shift 2 ;;
    --) shift; break ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$pending_share" ] && printf '%s\t\n' "$pending_share" >> "$shares_file"

if [ "$(uname -s)" != Linux ]; then
  echo "this installs the Linux server; on macOS use scripts/install-server.sh" >&2
  exit 1
fi
command -v systemctl >/dev/null || { echo "systemd is required" >&2; exit 1; }
command -v tailscale >/dev/null || echo "warning: the tailscale CLI was not found; homeportd needs Tailscale" >&2

if [ -z "$binary" ]; then
  cargo build --release --manifest-path "$repo/Cargo.toml" -p homeportd
  binary="$repo/target/release/homeportd"
fi

mkdir -p "$bin_dir"
# Copy to a temp name and rename so a running instance keeps its inode.
cp "$binary" "$bin_dir/homeportd.new"
chmod 755 "$bin_dir/homeportd.new"
mv -f "$bin_dir/homeportd.new" "$bin_dir/homeportd"
echo "installed $bin_dir/homeportd"

while IFS="$(printf '\t')" read -r path name; do
  [ -n "$path" ] && add_share "$path" "$name"
done < "$shares_file"

if ! "$bin_dir/homeportd" share list 2>/dev/null | grep -q '/'; then
  echo "no shares configured yet: add one with '$bin_dir/homeportd share add <path>'," >&2
  echo "then run '$bin_dir/homeportd service install'" >&2
  exit 1
fi

# Run from boot, not only while logged in.
if [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" != yes ]; then
  sudo loginctl enable-linger "$(id -un)" || echo "warning: could not enable lingering; the server only runs while you are logged in" >&2
fi

if [ $# -gt 0 ]; then
  "$bin_dir/homeportd" service install -- "$@"
else
  "$bin_dir/homeportd" service install
fi

state="${XDG_STATE_HOME:-$HOME/.local/state}/homeport/server"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [ -f "$state/cert.sha256" ] && break
  sleep 0.5
done
echo
"$bin_dir/homeportd" service status
[ -f "$state/cert.sha256" ] && echo "fingerprint: $(cat "$state/cert.sha256")"
