# Homebrew formula for the Homeport server, for the tap R44VC0RP/homebrew-tap:
#
#   brew install R44VC0RP/tap/homeportd
#
# It installs the prebuilt release binaries. After each release, run
# packaging/homebrew/update-formula.sh <tag> to set the version and checksums,
# then copy this file to Formula/homeportd.rb in the tap.
class Homeportd < Formula
  desc "Serves folders to the Homeport Finder app over Tailscale"
  homepage "https://github.com/R44VC0RP/homeport"
  version "0.5.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "homeportd", "homeport"
  end

  def caveats
    <<~EOS
      homeportd runs as your user (it serves your folders), so it is started
      with its own service command rather than `brew services`:

        homeportd share add ~/Projects      # each folder to share
        homeportd service install           # start now and at every login
        homeportd pair                      # link this server to the Mac app

      After `brew upgrade homeportd`, restart it to run the new version:

        #{OS.mac? ? "launchctl kickstart -k gui/$(id -u)/dev.vogel.homeportd" : "systemctl --user restart homeportd"}

      Tailscale must be installed and logged in on this machine.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/homeportd --version")
    (testpath/"server.toml").write "shares = []\n"
    (testpath/"shared").mkpath
    system bin/"homeportd", "--config", testpath/"server.toml", "share", "add", testpath/"shared"
    assert_match "shared", shell_output("#{bin}/homeportd --config #{testpath}/server.toml share list")
  end
end
