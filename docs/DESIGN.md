# Homeport

Homeport shows folders from another computer in Finder over Tailscale.
Browsing is instant, files download on demand, and a bad or missing
connection never hangs Finder.

This document started as the design for one setup (basebook serving
airbook) and keeps those names in examples. Since 0.5.0 the server also runs
on Linux and Windows and speaks protocol 3; see
[PROTOCOL.md](PROTOCOL.md) for the wire format and
[SECURITY.md](SECURITY.md) for the security model.

## Machines (the original setup)

- **basebook** (server): always-on Mac, tailnet IP `100.84.220.87`, macOS 27,
  case-insensitive APFS. Runs `homeportd`.
- **airbook** (client): MacBook Air that roams across good Wi-Fi, bad Wi-Fi,
  cellular and iPhone hotspot. Runs `Homeport.app` (menu bar app + File
  Provider extension).

## Decisions (agreed with Ryan)

- Shares for v1: `~/dev` (733 GB, hundreds of git repos) and `~/Downloads`.
- One File Provider domain named `basebook`; its root lists `dev` and
  `Downloads`.
- Use case: browse, open and edit files from Finder and editors. Git and
  builds stay on basebook over SSH. Running git inside the replica is not a
  goal.
- Offline: the sidebar item stays, with a clear Disconnected state. Anything
  already listed or downloaded still works. Anything else fails within ~1 s
  with `NSFileProviderError.serverUnreachable`. Never hang.
- Prediction may use: Finder's front window and selection (AppleScript),
  basebook activity (FSEvents, git, new Downloads), time of day.
- Cellular or hotspot (`isExpensive`) or Low Data Mode (`isConstrained`):
  prefetch folder listings only, never file contents.

## Architecture

```
basebook                                     airbook
┌──────────────────────────┐   QUIC over     ┌────────────────────────────────┐
│ homeportd (Rust)          │   tailnet       │ Homeport.app (Swift, menu bar) │
│  scanner (getattrlistbulk)│◄──────────────►│  link monitor, predictor,      │
│  index (SQLite, WAL)      │   1 stream/RPC │  Finder watcher, prefetcher    │
│  journal (FSEvents → seq) │                 │ HomeportFP.appex (Swift)       │
│  RPC server (quinn)       │                 │  NSFileProviderReplicatedExt.  │
│  thumbnails (QuickLook)   │                 │ homeport-core (Rust, UniFFI)   │
└──────────────────────────┘                 │  client, cache DB, chunk store │
                                             └────────────────────────────────┘
```

### Rust workspace (`crates/`)

- `homeport-proto`: messages (serde + postcard, length-prefixed), shared types.
- `homeportd`: server binary. Runs as a LaunchAgent (macOS), a systemd user
  unit (Linux) or a boot-time scheduled task (Windows), installed with
  `homeportd service install`. Platform code lives in
  `crates/homeportd/src/platform/{macos,linux,windows}`.
- `homeport-client`: async client library (quinn), link-quality estimator,
  priority scheduler, request coalescing. Used by the CLI and by Swift.
- `homeport-cli`: `homeport ls|stat|cat|changes|bench|status` for headless
  testing against a live server.
- `homeport-ffi`: UniFFI bindings over `homeport-client` for the Swift side.
- `homeport-netsim`: UDP impairment proxy for testing on bad networks.

### Transport

- Raw QUIC with quinn. One bidirectional stream per RPC; strict stream
  priorities (P0 user open > P1 visible listing > P2 thumbnails > P3 predicted
  prefetch > P4 background/upload).
- Tailscale MTU is 1280: `initial_mtu(1200)`, MTU discovery upper bound 1232.
- `keep_alive_interval` 5 s, `max_idle_timeout` 30 s. 0-RTT for idempotent
  reads only.
- TLS with a self-signed certificate generated on first run; the client pins
  its SHA-256 fingerprint.
- Server binds only to the tailnet IP. Every connection is checked with
  `tailscale whois --json <peer ip>`, cached; only allowed Tailscale logins
  (default: the server node's owner) get in, and tagged devices never do.
- Bulk content moves in 16–256 KiB pieces (about 100 ms of the link).
  Cancelling a File Provider `Progress` resets the stream.
- Congestion control: BBR (`HOMEPORT_CC` picks cubic/newreno), with the
  window held to ~2 BDP, and ~1 BDP while a P0/P1 request is running, so
  urgent answers do not wait behind a bulk transfer in the bottleneck queue.

### Server index and change journal

- Item identity: on macOS the APFS file ID (`ATTR_CMN_FILEID`), stable
  across renames; on Linux and Windows a number assigned by the index
  (inode and FileId are kept to follow renames). File Provider identifier =
  `"<id>"`; share roots are `root:<share>` (e.g. `root:dev`).
- SQLite (WAL, `synchronous=NORMAL`): `items(file_id PK, parent_id, name,
  kind, size, mtime_ns, ctime_ns, mode, rev, content_rev, hash NULL)` with
  indexes on `(parent_id, name)` and `rev`.
- Directories are listed with `getattrlistbulk` (macOS), `getdents64` +
  `statx` (Linux) or `FileIdExtdDirectoryInfo` (Windows) and cached
  pre-encoded per directory with a directory revision (one round trip "not
  modified").
- Lazy indexing: a directory is scanned the first time any client lists it,
  then kept current. A background crawler fills the rest of the "human tree"
  on idle. Generated trees (`node_modules`, `.git`, `target`, `build`,
  `dist`, `.next`, `DerivedData`, `.venv`, `__pycache__`) are indexed only
  when a client opens them.
- FSEvents (`FileEvents | NoDefer`, ~100 ms latency), resumed from the stored
  event ID; volume UUID stored; `MustScanSubDirs` / dropped events trigger a
  subtree rescan. Linux uses inotify on indexed directories, Windows
  `ReadDirectoryChangesExW`; both rescan after an overflow. FSEvents is only a hint: the anchor the client sees is the
  server's own monotonic `seq` plus a random `index_id`. A new `index_id`
  means the client must re-enumerate.
- The change feed is filtered to directories the client has enumerated
  (Perforce-style "have list"), so `npm install` on basebook does not flood
  airbook.

### Content

- Range reads for on-demand fetch (`NSFileProviderPartialContentFetching`
  for large files).
- zstd with level picked by link class (off/1 on Good, 3–6 on Degraded/Poor);
  skip known compressed media.
- Small-file bundles: one stream carrying many small files (< 64 KiB, up to
  ~8 MiB total) for prefetch.
- Later: BLAKE3 + FastCDC chunk manifests for resume and dedup.

### Writes

- Server-authoritative revisions. Uploads carry the base `content_rev`; a
  mismatch creates `name (airbook conflict YYYY-MM-DD HHMM).ext` beside it.
- Uploads are received into a staging file on the share's volume first.
  Content replacement then differs by platform:
  - macOS: the content is copied over the existing file in place (it keeps
    its APFS file ID), after an APFS clone of the old content as a crash
    backup, then `F_FULLFSYNC`.
  - Linux: a temp file in the same directory (mode and `user.*` xattrs
    copied), `fsync`, `renameat` over the target, `fsync` of the directory.
  - Windows: the staged file replaces the target with a POSIX-semantics
    rename (`FileRenameInformationEx`), keeping attributes and creation time.
  The item keeps its Homeport id in all three. Deletes go to the server's
  Trash (freedesktop Trash on Linux, Recycle Bin on Windows) and are refused
  when that isn't possible.

### Link quality (client)

- Inputs: `NWPathMonitor` on the physical interface (`isExpensive`,
  `isConstrained`, interface type), QUIC RTT/loss stats, app-level 1-byte ping
  (every 5 s while active, 30–60 s idle), `tailscale status --json` for
  direct vs DERP.
- States: Good, Degraded (RTT > 300 ms, loss > 3 %, or < 2 Mbps), Poor
  (RTT > 270 ms *and* < 700 kbps, or loss > 6 % when no transfer measured
  the rate, or RTT > 1.4 s), Offline.
- RTT, loss and throughput are windowed (min RTT over 10 s, loss over
  30 s excluding outages, best delivery rate while a transfer runs), so the
  state follows the link of the last few seconds.
- Hysteresis: drop to Offline after 2 back-to-back missed pings (~3 s on a
  fast link) or an unsatisfied path; one answered ping lifts Offline to
  Degraded, and a better class must hold 8 s before the state climbs.
- Deadlines are progress-based: a request fails when no bytes arrive for
  ~1 s (stretched to ~5 RTT on long paths), not when a big response is
  slow, and at once when the link is Offline.
- Offline: listings answer from the local copy; fetches of missing data fail
  fast with `serverUnreachable`. On reconnect call
  `signalErrorResolved(.serverUnreachable)` and signal the working set.
- Shown in the menu bar and as a decoration on the domain root.

### Prediction and prefetch

- Event log: enumerations, fetches (only `isFileViewerRequest` ones teach the
  model), thumbnails, Finder front-window path and selection (polled 2–4 Hz
  while Finder is frontmost), link class, time.
- Ranker: weighted log-linear mix of frecency (zoxide-style), first-order
  Markov folder transitions with decay, per-parent child counts with
  j-out-of-k confidence, name-pattern prior pooled across repos (`src`,
  `README.md`, `package.json`, ...), basebook recency, time-of-day bucket.
- Value of a prefetch = P(used) × latency saved ÷ (bytes × λ), λ from link
  class. Batched: one RPC for many folders.
- Tiers: always (even cellular) — change feed and one level of children of
  the current folder; Good — metadata of the human tree; Wi-Fi — small files
  in top-k predicted folders, READMEs/manifests, newest Downloads; Wi-Fi + AC
  — larger files and pinned folders.
- Thumbnails come from the server (`QLThumbnailGenerator` on macOS,
  Explorer's thumbnail cache on Windows, the `image` crate plus `ffmpeg` and
  `pdftoppm` when installed on Linux) through `NSFileProviderThumbnailing`,
  so Finder never downloads a file to draw it.

### Metrics

Time-to-listing p50/p95/p99 tagged hit-fresh / hit-stale / miss, stalls
> 1 s, first-open content hit rate, prefetch precision, cellular MB/day,
predictor hit@1/hit@5. Exposed by `homeport status` and the menu bar.
