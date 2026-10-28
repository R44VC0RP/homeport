# Security

## Reporting a vulnerability

Please report security problems privately through GitHub's
[private vulnerability reporting](https://github.com/R44VC0RP/homeport/security/advisories/new)
rather than a public issue. Include the version (`homeportd --version`),
the server OS and steps to reproduce.

## Model

Homeport gives a Mac read and write access to folders on another computer
the same person owns. The server trusts a client device only when Tailscale
says it belongs to an allowed login; the client trusts a server only when it
presents the certificate pinned at pairing.

### Who can connect

1. **Network.** `homeportd` binds to its tailnet IPv4 address (from
   `tailscale ip -4`), so it is not reachable from the LAN or the internet
   unless you pass `--bind`. On Windows, `homeportd service install` also
   adds a firewall rule that allows its UDP port only from `100.64.0.0/10`
   and `fd7a:115c:a1e0::/48`. Tailscale ACLs can narrow it further.
2. **Tailscale identity.** Before a QUIC connection is accepted, the server
   runs `tailscale whois` on the peer's address (results cached for 10
   minutes, refusals for 30 s). The peer is allowed only when its device is
   owned by a login in the allowlist:
   - `allow_logins` in `server.toml` plus `--allow-login` flags, or
   - when both are empty, the login that owns the server's own device.
   Tagged devices are never allowed. A tagged server must name its allowed
   logins explicitly. If Tailscale can't be asked, peers are refused.
   Loopback and the server's own addresses are always allowed (local
   testing with the `homeport` CLI).
3. **Pairing.** `homeportd pair` prints a one-time `homeport://` link and a
   6-digit code that let the Mac app learn the server's address and
   certificate fingerprint without copying them by hand. The code only
   helps a device that already passed checks 1 and 2.

### Server authentication

On first run the server generates a self-signed certificate and key in its
state directory (the key file is readable only by the user). Clients pin the
certificate's SHA-256 fingerprint and refuse any other certificate, so a
device that takes over the server's tailnet address can't impersonate it.
Deleting the state directory makes a new certificate; clients must pair
again.

### What a client can do

- Read, create, change, rename and move files inside the shared folders,
  and move items to the server's Trash (Recycle Bin on Windows).
- Every path is resolved inside its share: names with `/`, `\0`, `.` or
  `..` are refused, Linux opens use `openat2(RESOLVE_BENEATH |
  RESOLVE_NO_SYMLINKS)`, and symlink targets that leave the share are
  reported as `OutsideShare` rather than followed.
- Clients never permanently delete. A delete the server can't send to the
  Trash (no Trash on that volume, Recycle Bin off or too small) is refused.
- Edits made from an out-of-date copy never overwrite the server's version;
  they are kept beside it as a conflict copy.
- The server runs as the user who installed it, with that user's file
  permissions. It needs no root or administrator rights to run (only the
  Windows installer needs an elevated shell, to register the boot task and
  the firewall rule).

### Data in transit and at rest

All traffic is inside Tailscale (WireGuard) and additionally inside QUIC's
TLS 1.3. The server keeps an index of file names and metadata (SQLite) and a
thumbnail cache in the user's state and cache directories. The Mac app keeps
listings and downloaded files in its File Provider storage, managed by
macOS.

## Out of scope

- Someone who controls an allowed Tailscale login, or the tailnet's admin
  console, can connect.
- Anything with the server user's privileges on the server can read the
  shares and the key.
- Homeport does not encrypt files at rest; use FileVault, LUKS or BitLocker.
