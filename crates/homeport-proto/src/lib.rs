//! Homeport wire protocol.
//!
//! Every RPC uses one bidirectional QUIC stream. The client writes exactly one
//! [`Request`] frame and finishes its send side; the server answers with one or
//! more [`Response`] frames and finishes. A frame is a big-endian `u32` length
//! followed by that many bytes of postcard-encoded payload.

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 1;
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
    // Phase 2+: answered with `Error { code: Unsupported }` for now.
    Write {
        id: ItemId,
        base_content_rev: u64,
        offset: u64,
        len: u64,
    },
    Create {
        parent_id: ItemId,
        name: String,
        kind: Kind,
    },
    Rename {
        id: ItemId,
        new_parent_id: ItemId,
        new_name: String,
        base_rev: u64,
    },
    Delete {
        id: ItemId,
        base_rev: u64,
    },
    Thumbnail {
        id: ItemId,
        max_pixels: u32,
    },
    /// Answer: `LinkTarget`, or `Error { code: OutsideShare }` when the
    /// target leaves the item's share. Appended last to keep the encoding of
    /// earlier variants.
    ReadLink {
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
