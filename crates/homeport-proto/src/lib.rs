//! Homeport wire protocol.
//!
//! Every RPC uses one bidirectional QUIC stream. The client writes exactly one
//! [`Request`] frame and finishes its send side; the server answers with one or
//! more [`Response`] frames and finishes. A frame is a big-endian `u32` length
//! followed by that many bytes of postcard-encoded payload.

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 2;
/// TLS ALPN identifier.
pub const ALPN: &[u8] = b"homeport/1";
/// Default UDP port of `homeportd`.
pub const DEFAULT_PORT: u16 = 47820;
/// TLS server name used by clients (the certificate is pinned, not verified by name).
pub const SERVER_NAME: &str = "homeport";
/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Items per `Items` page.
pub const PAGE_SIZE: usize = 500;
/// Content chunk size for `Read`.
pub const READ_CHUNK: usize = 256 * 1024;

/// Identifier of the virtual container that lists every share.
pub const ROOT_ID: &str = "root";
/// Prefix of share root identifiers (`root:dev`, `root:Downloads`).
pub const SHARE_ID_PREFIX: &str = "root:";
/// Identifier of the virtual container listing items clients moved to
/// basebook's Trash (the File Provider trash container).
pub const TRASH_ID: &str = "trash";
/// Largest request header (the frame before an upload's content).
pub const MAX_REQUEST: usize = 1024 * 1024;

/// Item identifier: the APFS file ID in decimal, or `root` / `root:<share>`.
pub type ItemId = String;

/// Request class. Lower is more urgent; it maps to the QUIC stream priority.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// P0: the user is opening a file right now.
    UserOpen,
    /// P1: a listing Finder is showing.
    Visible,
    /// P2: thumbnails.
    Thumbnail,
    /// P3: predicted prefetch.
    Prefetch,
    /// P4: background work and uploads.
    Background,
}

impl Priority {
    /// QUIC stream priority (quinn sends higher values first).
    pub fn stream_priority(self) -> i32 {
        match self {
            Self::UserOpen => 4,
            Self::Visible => 3,
            Self::Thumbnail => 2,
            Self::Prefetch => 1,
            Self::Background => 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Item {
    pub id: ItemId,
    pub parent_id: ItemId,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    /// Server `seq` at which any field of this item last changed.
    pub rev: u64,
    /// Server `seq` at which its content (size or mtime) last changed.
    pub content_rev: u64,
    /// Creation time; 0 when unknown (rows indexed before protocol 2).
    pub crtime_ns: i64,
    /// BSD flags (`UF_HIDDEN`, ...).
    pub flags: u32,
    /// Raw `com.apple.metadata:_kMDItemUserTags` value (Finder tags), when known.
    #[serde(with = "serde_bytes")]
    pub tags: Option<Vec<u8>>,
}

/// Metadata a client asks the server to apply. `None` leaves a field alone.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub mtime_ns: Option<i64>,
    pub crtime_ns: Option<i64>,
    /// Permission bits (`0o7777`).
    pub mode: Option<u32>,
    pub hidden: Option<bool>,
    /// `Some(None)` removes the tags.
    pub tags: Option<Option<Vec<u8>>>,
    /// Extended attributes to set (syncable ones only; never removed).
    pub xattrs: Vec<(String, Vec<u8>)>,
}

impl Meta {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub priority: Priority,
    pub body: RequestBody,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ListDirReq {
    pub id: ItemId,
    /// The directory revision the client already has.
    pub if_dir_rev: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum RequestBody {
    /// First request on every connection.
    Hello {
        protocol: u32,
        client_id: String,
    },
    /// Answer: `NotModified`, or `Items` pages ending with `last: true`.
    ListDir(ListDirReq),
    /// Answer: for each directory, `NotModified` or its `Items` pages, then `End`.
    ListMany(Vec<ListDirReq>),
    /// Answer: `Item`.
    Stat {
        id: ItemId,
    },
    /// Answer: `Data` chunks then `DataEnd`. `len == 0` reads to end of file.
    Read {
        id: ItemId,
        offset: u64,
        len: u64,
        compress: bool,
    },
    /// Answer: `ChangeBatch` frames (or `SyncAnchorExpired`). With `follow`
    /// the stream stays open and a batch is sent whenever the index changes.
    Changes {
        since_seq: u64,
        index_id: u64,
        follow: bool,
    },
    /// Answer: `Pong`.
    Ping {
        nonce: u64,
    },
    Thumbnail {
        id: ItemId,
        max_pixels: u32,
    },
    /// Answer: `LinkTarget`, or `Error { code: OutsideShare }` when the
    /// target leaves the item's share.
    ReadLink {
        id: ItemId,
    },
    /// Creates a file, folder or symlink. A file's `size` bytes of content
    /// follow the request frame on the same stream. Answer: `Item`, or
    /// `Collision` when the name is taken. Replaying an `op_id` answers the
    /// item it created.
    Create {
        op_id: String,
        parent_id: ItemId,
        name: String,
        kind: Kind,
        size: u64,
        link_target: Option<String>,
        meta: Meta,
        /// The item may already exist (re-import): an existing folder, or a
        /// file with the same content, is answered instead of a collision.
        may_exist: bool,
    },
    /// Replaces a file's content in place (its id stays). `size` bytes
    /// follow. When `base_content_rev` is not the current content, the
    /// upload is kept as a conflict copy beside it. Answer: `Written`.
    WriteContent {
        id: ItemId,
        /// `None`: the client has no base version (skip the check).
        base_content_rev: Option<u64>,
        size: u64,
        meta: Meta,
        /// Shown in conflict copy names ("name (airbook conflict ...)").
        device: String,
    },
    /// Renames, moves and/or changes metadata. Answer: `Item` or `Collision`.
    Modify {
        id: ItemId,
        new_parent_id: Option<ItemId>,
        new_name: Option<String>,
        meta: Meta,
    },
    /// Moves an item to basebook's Trash. With `listed` it shows in the
    /// trash container (`TRASH_ID`) for Put Back. Non-empty folders need
    /// `recursive`. Answer: `Item` (its parent is `TRASH_ID` when listed).
    Trash {
        id: ItemId,
        listed: bool,
        recursive: bool,
    },
    /// Puts a trashed item back. Answer: `Item` or `Collision`.
    Untrash {
        id: ItemId,
        parent_id: ItemId,
        name: String,
    },
    /// Drops an item from the trash container; it stays in basebook's Trash.
    /// Answer: `End`.
    Forget {
        id: ItemId,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Deleted {
    pub id: ItemId,
    pub parent_id: ItemId,
    pub seq: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    NotFound,
    NotADirectory,
    NotAFile,
    OutsideShare,
    BadRequest,
    VersionMismatch,
    Unsupported,
    Internal,
    /// A folder is not empty.
    NotEmpty,
    /// The disk is full.
    NoSpace,
    /// The name is not allowed (`/`, `..`, too long, ...).
    BadName,
    /// The filesystem refused (permissions).
    Denied,
    /// The upload ended before all its bytes arrived.
    Incomplete,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Hello {
        protocol: u32,
        index_id: u64,
        seq: u64,
        roots: Vec<Item>,
    },
    NotModified {
        dir_id: ItemId,
        dir_rev: u64,
    },
    Items {
        dir_id: ItemId,
        dir_rev: u64,
        /// Total entries in the directory (across all pages).
        total: u32,
        page: u32,
        last: bool,
        items: Vec<Item>,
    },
    Item(Item),
    /// One directory of a `ListMany` failed.
    ListError {
        dir_id: ItemId,
        code: ErrorCode,
        message: String,
    },
    Data {
        offset: u64,
        /// The chunk is zstd-compressed; `raw_len` is its decoded size.
        compressed: bool,
        raw_len: u32,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    DataEnd {
        /// Bytes sent (decoded).
        total: u64,
        /// File size at the time of the read.
        size: u64,
        content_rev: u64,
    },
    ChangeBatch {
        /// The client's new anchor.
        seq: u64,
        changed: Vec<Item>,
        deleted: Vec<Deleted>,
        /// The batch brings the client up to the server's current seq.
        caught_up: bool,
    },
    SyncAnchorExpired {
        index_id: u64,
        seq: u64,
    },
    Pong {
        nonce: u64,
    },
    End,
    Error {
        code: ErrorCode,
        message: String,
    },
    /// A symlink's target, relative to the link's directory and inside its share.
    LinkTarget {
        target: String,
    },
    /// The name is taken (case-insensitively) by `existing`.
    Collision {
        existing: Item,
    },
    /// Answer to `WriteContent`. On a conflict `item` is the server's own
    /// (unchanged) version and `conflict` the copy holding the upload.
    Written {
        item: Item,
        conflict: Option<Item>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("encode: {0}")]
    Encode(postcard::Error),
    #[error("decode: {0}")]
    Decode(postcard::Error),
}

/// Encodes `msg` as one length-prefixed frame.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Bytes, FrameError> {
    let mut buf = BytesMut::with_capacity(256);
    buf.put_u32(0);
    let mut writer = buf.writer();
    postcard::to_io(msg, &mut writer).map_err(FrameError::Encode)?;
    let mut buf = writer.into_inner();
    let len = buf.len() - 4;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let len = u32::try_from(len).map_err(|_| FrameError::TooLarge(len))?;
    buf[..4].copy_from_slice(&len.to_be_bytes());
    Ok(buf.freeze())
}

/// Decodes a frame payload (without its length prefix).
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    postcard::from_bytes(payload).map_err(FrameError::Decode)
}

/// Checks a frame length prefix.
pub fn check_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

/// File extensions whose content is already compressed; never zstd them.
pub fn is_precompressed(name: &str) -> bool {
    const EXT: &[&str] = &[
        "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "dmg", "pkg", "jpg", "jpeg", "png", "gif", "webp", "heic",
        "avif", "mp4", "mov", "m4v", "mkv", "webm", "mp3", "m4a", "aac", "flac", "ogg", "opus", "pdf", "woff", "woff2",
        "jar", "apk", "ipa", "xip", "br", "lz4",
    ];
    name.rsplit_once('.').is_some_and(|(_, ext)| EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Names a client never uploads: Finder metadata and editor temporary files
/// (TextEdit `.sb-` saves, vim swap and `4913` probes, JetBrains and Office
/// temporaries, backups ending in `~`).
pub fn is_junk(name: &str) -> bool {
    if matches!(name, ".DS_Store" | "Icon\r" | "4913" | ".localized") || name.starts_with("._") {
        return true;
    }
    if name.contains(".sb-") || name.ends_with('~') || name.starts_with("~$") {
        return true;
    }
    if name.starts_with("~WRL") && name.ends_with(".tmp") {
        return true;
    }
    if name.starts_with(".~lock.") && name.ends_with('#') {
        return true;
    }
    if name.ends_with("___jb_tmp___") || name.ends_with("___jb_old___") {
        return true;
    }
    // vim swap files: .name.swp ... .name.swa (and .swo, .swn, ...).
    let b = name.as_bytes();
    b.len() > 5 && b[0] == b'.' && name[..b.len() - 1].ends_with(".sw") && (b'a'..=b'p').contains(&b[b.len() - 1])
}

/// Whether `name` is acceptable as one path component on the server.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0') && name.len() <= 255
}
