# Homeport

Homeport shows basebook's `~/dev` and `~/Downloads` in Finder on airbook,
over Tailscale. Browsing is instant, files download on demand, and a bad or
missing connection never hangs Finder.

## Machines

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
- `homeportd`: server binary. Runs as a LaunchAgent on basebook.
- `homeport-client`: async client library (quinn), link-quality estimator,
  priority scheduler, request coalescing. Used by the CLI and by Swift.
- `homeport-cli`: `homeport ls|stat|cat|changes|bench|status` for headless
  testing against a live server.
- `homeport-ffi`: UniFFI bindings over `homeport-client` for the Swift side.

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
  `tailscale whois --json <peer ip>`, cached; only Ryan's own user is allowed.
- Bulk content moves in 256 KiB–1 MiB pieces so a P0 request is never queued
  behind a big transfer. Cancelling a File Provider `Progress` resets the
  stream.

### Server index and change journal

- Item identity: APFS file ID (`ATTR_CMN_FILEID`), stable across renames.
  File Provider identifier = `"<fileid>"`; the two share roots are
  `root:dev` and `root:Downloads`.
- SQLite (WAL, `synchronous=NORMAL`): `items(file_id PK, parent_id, name,
  kind, size, mtime_ns, ctime_ns, mode, rev, content_rev, hash NULL)` with
  indexes on `(parent_id, name)` and `rev`.
- Directories are listed with `getattrlistbulk` and cached pre-encoded per
  directory with a directory revision (one round trip "not modified").
- Lazy indexing: a directory is scanned the first time any client lists it,
  then kept current. A background crawler fills the rest of the "human tree"
  on idle. Generated trees (`node_modules`, `.git`, `target`, `build`,
  `dist`, `.next`, `DerivedData`, `.venv`, `__pycache__`) are indexed only
  when a client opens them.
- FSEvents (`FileEvents | NoDefer`, ~100 ms latency), resumed from the stored
  event ID; volume UUID stored; `MustScanSubDirs` / dropped events trigger a
  subtree rescan. FSEvents is only a hint: the anchor the client sees is the
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
- Server writes go to a temp file in the same directory, `F_FULLFSYNC`,
  re-check, then `rename`. Deletes go to basebook's Trash.

### Link quality (client)

- Inputs: `NWPathMonitor` on the physical interface (`isExpensive`,
  `isConstrained`, interface type), QUIC RTT/loss stats, app-level 1-byte ping
  (every 5 s while active, 30–60 s idle), `tailscale status --json` for
  direct vs DERP.
- States: Good, Degraded (RTT > 300 ms, loss > 3 %, or < 2 Mbps), Poor
  (RTT > 270 ms *and* < 700 kbps, or RTT > 1.4 s), Offline.
- Hysteresis: drop to Offline after ~3 missed pings (6–10 s) or an
  unsatisfied path; climb back only after 2–3 good probes over 15–30 s.
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
- Thumbnails come from basebook (`QLThumbnailGenerator`) through
  `NSFileProviderThumbnailing`, so Finder never downloads a file to draw it.

### Metrics

Time-to-listing p50/p95/p99 tagged hit-fresh / hit-stale / miss, stalls
> 1 s, first-open content hit rate, prefetch precision, cellular MB/day,
predictor hit@1/hit@5. Exposed by `homeport status` and the menu bar.
