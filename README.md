# Homeport

Homeport puts folders from another computer in your Mac's Finder sidebar,
over [Tailscale](https://tailscale.com). Browsing is instant, files download
when you open them, edits are saved back to the other computer, and a slow or
missing connection never hangs Finder. The other computer (the server) can be
a Mac, a Linux machine or a Windows PC.

## How it works

- **homeportd** runs on the server. It indexes the folders you share (in
  SQLite, kept current with FSEvents, inotify or Windows change
  notifications) and answers requests over QUIC, bound only to the server's
  tailnet address.
- **Homeport.app** runs on your Mac: a menu bar app plus a File Provider
  extension, so each server shows up in Finder like iCloud Drive does. It
  caches listings, fetches file contents on demand, prefetches what you are
  likely to open next, and uploads your edits.
- The connection is checked twice: Tailscale tells the server which login
  owns the connecting device, and the app pins the server's certificate.

More detail: [docs/DESIGN.md](docs/DESIGN.md) (architecture),
[docs/PROTOCOL.md](docs/PROTOCOL.md) (wire protocol),
[docs/SECURITY.md](docs/SECURITY.md) (security model).

## Requirements

- Tailscale on both computers, logged in to the same tailnet.
- The Mac app (client): macOS 26 or later, Apple silicon.
- The server: one of the systems below.

| Server | Status | Caveats |
| --- | --- | --- |
| macOS 13+ (Apple silicon or Intel; developed on macOS 26) | Supported | Shared folders in `~/Downloads`, `~/Documents` or `~/Desktop` need a one-time privacy permission (see [Troubleshooting](#troubleshooting)). |
| Linux, x86_64 or arm64 (ext4, btrfs, xfs, zfs), systemd | Supported | Creation dates set from the Mac are not kept (Linux can't set them). The Hidden flag is not kept (Linux uses dot names). Finder tags and extended attributes need a filesystem with `user.*` xattrs. Large shares need more inotify watches. |
| Windows 10/11, x86_64 (NTFS) | Supported | Finder tags, extended attributes and POSIX permissions are not kept. Creating symlinks needs Developer Mode. OneDrive files that are only in the cloud are listed but never downloaded by the server. Tailscale must run unattended. |

On every server, deleting from Finder moves items to the server's Trash
(Recycle Bin on Windows), never deletes them outright; a delete the server
can't send to the Trash is refused.

## Install

Install a server first, then the Mac app, then pair them.

### Server: Linux

```sh
curl -fsSL https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-linux.sh | sh
```

It installs `homeportd` and `homeport` to `~/.local/bin`, asks for a folder
to share, and starts a systemd user service. Pass options after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-linux.sh |
  sh -s -- --share ~/Projects --share /srv/media --name media --enable-linger
```

`--enable-linger` runs `sudo loginctl enable-linger $USER`, so the server
starts at boot instead of at your first login. `--version v0.6.0` pins a
release; `--help` lists the rest.

### Server: macOS

With [Homebrew](https://brew.sh) (once the tap is published):

```sh
brew install R44VC0RP/tap/homeportd
homeportd share add ~/Projects
homeportd service install
```

Or without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-macos.sh | sh
```

`homeportd service install` adds a LaunchAgent, so the server runs while you
are logged in (it must run in your login session to use the Trash, Quick Look
thumbnails and folder permissions). Keep the server Mac logged in, with
automatic login if it restarts unattended.

### Server: Windows

In PowerShell **as administrator**:

```powershell
irm https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-windows.ps1 | iex
```

With options:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-windows.ps1))) -Share C:\Users\me\Projects -Share 'D:\Media=media'
```

It installs to `%LOCALAPPDATA%\Programs\Homeport`, registers a scheduled task
that runs `homeportd` as you at boot whether or not you are logged on (S4U, no
stored password), and adds a firewall rule allowing its UDP port only from
tailnet addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`).

### All installers

They can be run again to upgrade: binaries are replaced in place, your
config and shares are never overwritten (shares are only added), and an
installed service is restarted with the arguments it had. Release archives
are checked against the release's `SHA256SUMS`.

To manage shares later:

```sh
homeportd share add <folder> [--name NAME]
homeportd share list
homeportd share remove NAME
homeportd service install    # restart to apply (also: service status, service uninstall)
```

The config file is `server.toml` in `~/.config/homeport` (Linux),
`~/Library/Application Support/Homeport` (macOS) or `%APPDATA%\Homeport`
(Windows). It also takes `port`, `name` and `allow_logins`.

### Mac app

1. Download `Homeport.dmg` from the
   [latest release](https://github.com/R44VC0RP/homeport/releases/latest),
   open it and drag Homeport to Applications.
2. Open Homeport. It lives in the menu bar.
3. If Finder shows no Homeport location, turn the extension on in System
   Settings > General > Login Items & Extensions > File Providers.

### Pair the Mac with a server

On the server, run:

```sh
homeportd pair
```

It prints a `homeport://` link and a 6-digit code. Open the link on your
Mac (or choose **Add Server…** in the Homeport menu and enter the code).
The app connects over Tailscale, checks the code, pins the server's
certificate, and the server appears in Finder's sidebar. Pair as many
servers as you like.

## Security model

- **Tailnet only.** The server binds to its Tailscale address, not to your
  LAN or the internet. On Windows the firewall rule also limits the port to
  tailnet addresses.
- **Tailscale login allowlist.** For each new peer the server asks
  `tailscale whois` which login owns the device. Only the login that owns the
  server is allowed by default; add others with `allow_logins` in
  `server.toml` or `--allow-login`. Tagged devices are never allowed.
- **Certificate pinning.** The server makes a self-signed certificate on
  first run. The app pins its SHA-256 fingerprint when pairing and refuses
  any other certificate.
- **Shares are confined.** Every path is resolved inside its share; `..`,
  absolute names and symlinks that leave the share are refused.
- **Nothing is permanently deleted** from the server by a client.

Details and reporting: [docs/SECURITY.md](docs/SECURITY.md).

## Troubleshooting

**No Homeport location in Finder.** Turn the extension on in System Settings
> General > Login Items & Extensions > File Providers, then quit and reopen
Homeport.

**macOS server: Downloads, Documents or Desktop is empty or the server hangs
at startup.** These folders are privacy-protected. The first run shows
"homeportd would like to access files in your Downloads folder": click Allow,
or turn it on later in System Settings > Privacy & Security > Files and
Folders > homeportd. Upgrading can ask again.

**Windows server unreachable after you log out.** Tailscale disconnects at
logout unless it runs unattended: Tailscale menu > Preferences > Run
unattended.

**Linux: changes on the server don't show up, or the log says "out of
inotify watches".** Raise the limit:

```sh
echo fs.inotify.max_user_watches=1048576 | sudo tee /etc/sysctl.d/90-homeport-inotify.conf
sudo sysctl --system
```

**Linux server only runs while you are logged in.** Turn on lingering:
`sudo loginctl enable-linger $USER`.

**Logs.** `journalctl --user -u homeportd -f` (Linux),
`~/Library/Logs/Homeport/homeportd.log` (macOS),
`%LOCALAPPDATA%\Homeport\logs\homeportd.log` (Windows). `homeportd service
status` shows whether it is running.

## Building from source

Rust (stable) for the server and CLI:

```sh
cargo build --release -p homeportd -p homeport-cli
```

The Mac app needs Xcode 26+ and [XcodeGen](https://github.com/yonaskolb/XcodeGen):

```sh
scripts/build-ffi.sh                 # Rust core as an XCFramework + Swift bindings
cd apple && xcodegen generate
xcodebuild -project Homeport.xcodeproj -scheme Homeport -configuration Release \
  CODE_SIGNING_ALLOWED=NO build      # unsigned; see scripts/build-app.sh for a signed build
```

A File Provider extension only loads when signed with a team that has the
app group, so a working local app needs your own team ID and app group in
`apple/project.yml`.

| Path | What |
| --- | --- |
| `crates/homeportd` | Server |
| `crates/homeport-proto` | Wire protocol |
| `crates/homeport-client` | Async client library |
| `crates/homeport-ffi` | Swift bindings for the app |
| `crates/homeport-cli` | `homeport`, a command-line client for testing a server |
| `crates/homeport-netsim` | UDP impairment proxy for testing on bad networks |
| `apple/` | Homeport.app and its File Provider extension |
| `scripts/` | Installers, build and release scripts |

Releases: push a tag `vX.Y.Z` matching the workspace version; the
[release workflow](.github/workflows/release.yml) builds the server archives
into a draft release, and the maintainer adds the notarized app with
`scripts/release-app.sh vX.Y.Z` before publishing.

## License

[Apache License 2.0](LICENSE).
