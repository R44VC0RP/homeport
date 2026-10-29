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
  version "0.6.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-aarch64-apple-darwin.tar.gz"
      sha256 "464148210db6fef832b1cea7e372f0d563446e1b5e1cde9b084c86395cc3bab3"
    end
    on_intel do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-x86_64-apple-darwin.tar.gz"
      sha256 "b1b17796de647c9d007f90a1f70baae1b3c86a7042dc549d72ff17d5de1c5f9a"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0a5a1932b9308c00983c831876238a1e5c86cc71f11af8f5a97bff7c2e7abcb9"
    end
    on_intel do
      url "https://github.com/R44VC0RP/homeport/releases/download/v#{version}/homeport-x86_64-unknown-linux-musl.tar.gz"
      sha256 "58b4fef9c2aff0f89a43d54405b3a96294267dd26c026e43f4440ab6fe0021b8"
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
