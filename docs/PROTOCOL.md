# Homeport wire protocol

Protocol version **3**. The source of truth is
[`crates/homeport-proto/src/lib.rs`](../crates/homeport-proto/src/lib.rs);
this page is a map of it.

## Transport

- QUIC (quinn) over UDP, default port **47820**, on the server's tailnet
  address.
- TLS 1.3, ALPN `homeport/1`, server name `homeport`. The server's
  certificate is self-signed; clients pin its SHA-256 fingerprint (of the
  DER certificate, lowercase hex) instead of verifying a chain or name. No
  client certificate: clients are identified by Tailscale (`tailscale whois`
  on the source address) before the connection is accepted.
- Initial MTU 1200, path MTU discovery up to 1232 (Tailscale's MTU is 1280).
  Keep-alive 5 s, idle timeout 30 s, up to 512 concurrent bidirectional
  streams, no unidirectional streams.

## Framing

Every RPC uses its own bidirectional stream. The client writes one
`Request` frame (an upload's content follows it raw on the same stream) and
finishes its send side. The server answers with one or more `Response`
frames and finishes.

A frame is a big-endian `u32` length followed by that many bytes of
[postcard](https://docs.rs/postcard)-encoded payload. Limits: 8 MiB per
frame, 1 MiB for a request header.

## Requests

`Request { priority, body }`. The priority maps to the QUIC stream priority:
`UserOpen` (a file being opened) > `Visible` (a listing on screen) >
`Thumbnail` > `Prefetch` > `Background` (uploads, crawling).

| Request | Answer |
| --- | --- |
| `Hello { protocol, client_id }` (first on every connection) | `Hello { protocol, index_id, seq, roots, server }`, or `Error { VersionMismatch }` |
| `ListDir { id, if_dir_rev }` | `NotModified`, or `Items` pages (500 items each) ending with `last: true` |
| `ListMany([ListDir])` | per directory `NotModified`, `Items` pages or `ListError`; then `End` |
| `Stat { id }` | `Item` |
| `Read { id, offset, len, compress }` | `Data` chunks (zstd when asked and worthwhile), then `DataEnd { total, size, content_rev }` |
| `Changes { since_seq, index_id, follow }` | `ChangeBatch { seq, changed, deleted, caught_up }` frames, or `SyncAnchorExpired`; with `follow` the stream stays open |
| `Ping { nonce }` | `Pong` |
| `Thumbnail { id, max_pixels }` | PNG in `Data` chunks, then `DataEnd` (zero bytes: no thumbnail) |
| `ReadLink { id }` | `LinkTarget`, or `Error { OutsideShare }` |
| `Create { op_id, parent_id, name, kind, size, link_target, meta, may_exist }` + content | `Item` or `Collision`; replaying an `op_id` answers the same item |
| `WriteContent { id, base_content_rev, size, meta, device }` + content | `Written { item, conflict }` |
| `Modify { id, new_parent_id, new_name, meta }` | `Item` or `Collision` |
| `Trash { id, listed, recursive }` | `Item` (parent `trash` when listed) |
| `Untrash { id, parent_id, name }` | `Item` or `Collision` |
| `Forget { id }` | `End` |

Any request can be answered with `Error { code, message }`. Codes:
`NotFound`, `NotADirectory`, `NotAFile`, `OutsideShare`, `BadRequest`,
`VersionMismatch`, `Unsupported`, `Internal`, `NotEmpty`, `NoSpace`,
`BadName`, `Denied`, `Incomplete`, `Busy` (Windows sharing violation, retry),
`CloudOnly` (a cloud placeholder the server will not download).

## Identity and change tracking

- Item ids are strings: a decimal number assigned by the server's index, or
  `root` (the list of shares), `root:<share>` (a share's root) and `trash`
  (items clients moved to the server's Trash). Numbers are stable for the
  life of the index; a macOS server uses APFS file IDs.
- The server's index has a random `index_id` and a monotonic `seq`. Every
  item carries `rev` (seq of its last change) and `content_rev` (seq of its
  last content change). A client resumes the change feed from its last
  `seq`; a different `index_id`, or `SyncAnchorExpired`, means it must list
  everything again.
- The change feed only reports directories the client has listed.
- `WriteContent` replaces a file's content and keeps its id. When
  `base_content_rev` is not the current content, the upload is kept beside
  it as `name (<device> conflict YYYY-MM-DD HHMM).ext` and both are returned.

## Server info

`Hello` carries `ServerInfo { name, os, version, case_sensitive,
normalization_sensitive, caps }`. Capability bits (set only when every share
has them): `THUMBNAILS`, `TRASH`, `XATTRS`, `TAGS`, `SYMLINKS`,
`SET_CRTIME`, `HIDDEN`, `MODE`. Clients use them to avoid sending metadata a
server would drop.

`Item.flags`: `READ_ONLY`, `HIDDEN`, `NAME_ALIASED` (the name was changed to
avoid a case or Unicode-normalization collision with a sibling),
`NAME_UNREPRESENTABLE` (the raw name is not valid Unicode), `CLOUD_ONLY`.

## Compatibility

`PROTOCOL_VERSION` is bumped on any incompatible change, and a server only
accepts clients speaking its own version (others get
`Error { VersionMismatch }` in answer to `Hello`). Version 3 added
`ServerInfo` with capabilities for Linux and Windows servers.
