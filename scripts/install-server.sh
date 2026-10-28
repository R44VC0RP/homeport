#!/bin/sh
# Builds homeportd (release) and installs it as a LaunchAgent. Does not load it.
#
#   scripts/install-server.sh            build, install binary, write plist
#   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.vogel.homeportd.plist
#   launchctl bootout gui/$(id -u)/dev.vogel.homeportd
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
label=dev.vogel.homeportd
support="$HOME/Library/Application Support/Homeport"
bin_dir="$support/bin"
log_dir="$HOME/Library/Logs/Homeport"
plist="$HOME/Library/LaunchAgents/$label.plist"

cargo build --release --manifest-path "$repo/Cargo.toml" -p homeportd

mkdir -p "$bin_dir" "$log_dir" "$HOME/Library/LaunchAgents"
# Copy to a temp name and rename so a running instance keeps its inode.
cp "$repo/target/release/homeportd" "$bin_dir/homeportd.new"
mv -f "$bin_dir/homeportd.new" "$bin_dir/homeportd"

cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$bin_dir/homeportd</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <!-- Restarted if it exits, e.g. when Tailscale is not up yet at login. -->
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>$log_dir/homeportd.log</string>
  <key>StandardErrorPath</key>
  <string>$log_dir/homeportd.log</string>
</dict>
</plist>
EOF
plutil -lint "$plist" >/dev/null

echo "installed $bin_dir/homeportd"
echo "wrote $plist (not loaded)"
echo
echo "Load:   launchctl bootstrap gui/\$(id -u) '$plist'"
echo "Unload: launchctl bootout gui/\$(id -u)/$label"
echo "Logs:   $log_dir/homeportd.log"
echo "Fingerprint: $support/server/cert.sha256 (created on first run)"
echo
echo "Note: ~/Downloads is privacy-protected. The first run from launchd may"
echo "prompt for access, or need homeportd added under Privacy & Security >"
echo "Files and Folders (or Full Disk Access)."
