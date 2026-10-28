//! Everything that differs between server operating systems.
//!
//! One module per OS (`macos`, `linux`, `windows`) implements the traits
//! below on a few concrete types and re-exports them under fixed names:
//!
//! | name          | trait          | what it is                                   |
//! |---------------|----------------|----------------------------------------------|
//! | `Fs`          | [`FsOps`]      | path operations, error mapping, share caps    |
//! | `Dir`         | [`DirOps`]     | an open directory; every write goes through it |
//! | `File`        | [`FileOps`]    | an open file or item handle                    |
//! | `Volume`      | [`VolumeOps`]  | a mounted filesystem; key → path               |
//! | `Watcher`     | [`WatcherOps`] | change notifications with a resumable cursor   |
//! | `Trash`       | [`TrashOps`]   | the OS trash (never a permanent delete)        |
//! | `Thumbnailer` | [`ThumbnailerOps`] | PNG thumbnails made on the server          |
//! | `Sys`         | [`SysOps`]     | directories, host name, Tailscale CLI, signals |
//! | `Service`     | [`ServiceOps`] | `homeportd service install/uninstall/status`   |
//!
//! Dispatch is static (no `dyn`); [`conformance`] fails to compile when a
//! platform misses part of the contract. Shared code (index, server, auth,
//! names) only uses these names and never `libc` or `std::os::*`.
//!
//! Linux and Windows are stubs that return [`unsupported`] errors; their doc
//! comments say how each piece should be built (see the porting brief).

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use homeport_proto::{Kind, Os};
use tokio::sync::mpsc;

#[cfg(unix)]
pub mod unix;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

pub use imp::{Dir, File, Fs, Service, Sys, Thumbnailer, Trash, Volume, Watcher};

/// Identity of a file as the OS reports it. Only a *hint*: the index owns
/// item identity (see `index::ids`), because on Linux and Windows most saves
/// replace the file (new inode / FileId) and inode numbers get reused.
///
/// - macOS: `vol` = `st_dev`, `id` = APFS file id, `generation` = 0.
/// - Linux: `vol` = `st_dev` (or `stx_mnt_id`), `id` = inode, `generation` = birth
///   time in ns (a reused inode gets a new btime), 0 where unknown.
/// - Windows: `vol` = volume serial number, `id` = 128-bit `FILE_ID_128`,
///   `generation` = 0 (the NTFS file reference number embeds a sequence number).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub vol: u64,
    pub id: u128,
    pub generation: u64,
}

impl FileKey {
    /// Whether two keys name the same file (a zero `generation` matches any).
    pub fn same_file(&self, other: &FileKey) -> bool {
        self.vol == other.vol
            && self.id == other.id
            && (self.generation == 0 || other.generation == 0 || self.generation == other.generation)
    }

    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// 32-byte big-endian encoding for the database.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(32);
        v.extend_from_slice(&self.vol.to_be_bytes());
        v.extend_from_slice(&self.id.to_be_bytes());
        v.extend_from_slice(&self.generation.to_be_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != 32 {
            return None;
        }
        Some(Self {
            vol: u64::from_be_bytes(b[0..8].try_into().ok()?),
            id: u128::from_be_bytes(b[8..24].try_into().ok()?),
            generation: u64::from_be_bytes(b[24..32].try_into().ok()?),
        })
    }
}

/// A name exactly as stored on disk. Never derived from a display name
/// except through [`RawName::from_display`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RawName(pub OsString);

impl RawName {
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Bytes for the database: the raw bytes on Unix, UTF-16LE on Windows.
    pub fn to_bytes(&self) -> Vec<u8> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.0.as_bytes().to_vec()
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            self.0.encode_wide().flat_map(u16::to_le_bytes).collect()
        }
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self(OsStr::from_bytes(b).to_owned())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let wide: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            Self(OsString::from_wide(&wide))
        }
    }

    /// The name shown to clients and its [`homeport_proto::item_flags`]
    /// (`NAME_UNREPRESENTABLE`). Collision aliases are added later, per
    /// directory, by `names::assign_aliases`.
    pub fn display(&self) -> (String, u32) {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            if let Some(s) = self.0.to_str() {
                return (s.to_owned(), 0);
            }
            crate::names::display_bytes(self.0.as_bytes())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = self.0.encode_wide().collect();
            crate::names::display_wide(&wide)
        }
    }

    /// The raw name for a display name a client sent, per the platform's
    /// [`NamePolicy`] (escapes undone, NFC on Linux/Windows, SFM mapping of
    /// characters Windows forbids).
    pub fn from_display(name: &str) -> Result<Self, crate::names::NameError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            crate::names::bytes_from_display(name, Fs::NAME_POLICY).map(|b| Self(OsString::from_vec(b)))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            crate::names::wide_from_display(name).map(|w| Self(OsString::from_wide(&w)))
        }
    }

    /// The last component of a path.
    pub fn of_path(path: &Path) -> Option<Self> {
        path.file_name().map(|n| Self(n.to_owned()))
    }
}

/// How a platform turns client names into names on disk.
#[allow(dead_code, reason = "each platform uses one")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePolicy {
    /// APFS/HFS+: names are kept as sent (the filesystem folds case and
    /// normalization itself).
    Apple,
    /// Linux: bytes; new names are NFC-normalized.
    Posix,
    /// Windows/NTFS: UTF-16; NFC; forbidden characters mapped to the
    /// SFM/Samba private-use characters; reserved device names refused.
    Windows,
}

/// Attributes of one directory entry as the filesystem reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: FileKey,
    pub name: RawName,
    pub kind: Kind,
    /// Bytes; 0 for directories.
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    /// Creation time; 0 when the filesystem has none.
    pub crtime_ns: i64,
    /// POSIX permission bits (`0o7777`); synthesized where the OS has none.
    pub mode: u32,
    /// [`homeport_proto::item_flags`] bits (HIDDEN, READ_ONLY, CLOUD_ONLY).
    pub flags: u32,
}

/// How a platform maps an item's identity to an `ItemId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdScheme {
    /// `ItemId` = `(volume slot << 56) | key.id` (macOS: APFS ids survive
    /// renames and `fsgetpath` resolves them). No id table.
    Direct,
    /// The index assigns ids and keeps key → id in its `ids` table,
    /// carrying an id across replace-by-rename saves (Linux, Windows).
    Assigned,
}

/// What a share's filesystem can do; probed once per share root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsCaps {
    /// `a` and `A` can both exist.
    pub case_sensitive: bool,
    /// NFC and NFD spellings of a name can both exist.
    pub normalization_sensitive: bool,
    pub symlinks: bool,
    pub xattrs: bool,
    pub tags: bool,
    pub set_crtime: bool,
    pub hidden: bool,
    pub mode: bool,
}

/// Durability for a written file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// On stable storage before we answer (macOS `F_FULLFSYNC`).
    Full,
    /// Ordered but cheap (macOS `F_BARRIERFSYNC`); for bulk uploads.
    Barrier,
}

/// Metadata to apply to an open item. `None` leaves a field alone.
pub struct SetMeta<'a> {
    pub mode: Option<u32>,
    pub hidden: Option<bool>,
    /// `Some(None)` removes the tags.
    pub tags: Option<Option<&'a [u8]>>,
    pub xattrs: &'a [(String, Vec<u8>)],
    pub crtime_ns: Option<i64>,
    pub mtime_ns: Option<i64>,
}

/// Platform-independent class of an I/O error.
#[allow(dead_code, reason = "not every platform reports every kind")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrKind {
    NotFound,
    NotADirectory,
    /// A symlink where a file or directory was expected (`ELOOP` from `O_NOFOLLOW`).
    Symlink,
    Exists,
    NotEmpty,
    NoSpace,
    Denied,
    BadName,
    /// In use by another process (Windows sharing violation); retryable.
    Busy,
    /// A cloud placeholder that would have to be downloaded.
    CloudOnly,
    /// Moving between volumes.
    CrossDevice,
    Unsupported,
    Other,
}

/// Classification from `io::ErrorKind` alone; platforms refine with raw codes.
pub fn classify_kind(e: &io::Error) -> ErrKind {
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound => ErrKind::NotFound,
        K::NotADirectory => ErrKind::NotADirectory,
        K::AlreadyExists => ErrKind::Exists,
        K::DirectoryNotEmpty => ErrKind::NotEmpty,
        K::StorageFull | K::QuotaExceeded => ErrKind::NoSpace,
        K::PermissionDenied | K::ReadOnlyFilesystem => ErrKind::Denied,
        K::InvalidFilename | K::InvalidInput => ErrKind::BadName,
        K::CrossesDevices => ErrKind::CrossDevice,
        K::ResourceBusy => ErrKind::Busy,
        K::Unsupported => ErrKind::Unsupported,
        _ => ErrKind::Other,
    }
}

/// The error every unimplemented platform function returns.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("{what} is not implemented on {} yet", Fs::OS_NAME))
}

/// Paths and their file operations.
pub trait FsOps {
    const OS: Os;
    /// For messages (`unsupported`).
    #[allow(dead_code)]
    const OS_NAME: &'static str;
    const ID_SCHEME: IdScheme;
    const NAME_POLICY: NamePolicy;

    /// `lstat`: never follows a symlink.
    fn lstat(path: &Path) -> io::Result<Entry>;
    /// Opens a regular file for reading without following a symlink
    /// (`ErrKind::Symlink` when it is one). Must not hydrate cloud
    /// placeholders (`ErrKind::CloudOnly`).
    fn open_read(path: &Path) -> io::Result<File>;
    /// Finder tags of an item (raw `com.apple.metadata:_kMDItemUserTags`
    /// plist), if any.
    fn tags(path: &Path) -> Option<Vec<u8>>;
    /// Classifies an error from any function in this module.
    fn classify(e: &io::Error) -> ErrKind;
    /// Probes a share root's filesystem.
    fn caps(root: &Path) -> FsCaps;
    /// Entries never shown to clients (`.DS_Store`; Windows `desktop.ini`,
    /// `Thumbs.db`, `$RECYCLE.BIN`, ...).
    fn skip(name: &OsStr) -> bool;
    /// Content of `upload` replaces that of `name` in `dir`, applying `meta`
    /// and syncing fully. Keeps the item's identity where the OS allows (macOS:
    /// in place, with a clone as crash backup in `staging`); elsewhere a temp
    /// file + atomic replace is fine, the index carries the id over. Returns
    /// the item's new attributes.
    fn replace_content(dir: &Dir, name: &OsStr, upload: &File, staging: &Dir, meta: &SetMeta<'_>) -> io::Result<Entry>;
}

/// An open directory. Every operation is relative to it and never follows a
/// symlink in the last component; names are single components.
pub trait DirOps: Sized + Send + Sync {
    /// Opens a directory path (not following a final symlink). With
    /// `expect`, fails with `NotFound` unless it is still that directory.
    fn open(path: &Path, expect: Option<&FileKey>) -> io::Result<Self>;
    /// Every entry with its attributes (fast path: one bulk call per buffer).
    fn scan(&self) -> io::Result<Vec<Entry>>;
    /// `lstat` of one entry.
    fn stat(&self, name: &OsStr) -> io::Result<Entry>;
    /// The entry's name as stored on disk (differs in case or normalization
    /// on insensitive filesystems).
    fn stored_name(&self, name: &OsStr) -> io::Result<RawName>;
    /// Opens a regular file (read, or read-write).
    fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File>;
    /// A handle to apply metadata to an item of any kind (directories, and
    /// symlinks themselves).
    fn open_meta(&self, name: &OsStr, kind: Kind) -> io::Result<File>;
    fn mkdir(&self, name: &OsStr) -> io::Result<()>;
    fn symlink(&self, target: &str, name: &OsStr) -> io::Result<()>;
    fn unlink(&self, name: &OsStr) -> io::Result<()>;
    /// Renames without replacing: `Exists` when `to` exists (a case-only
    /// rename of the same item is allowed).
    fn rename_noreplace(&self, from: &OsStr, to_dir: &Dir, to: &OsStr) -> io::Result<()>;
    /// Copy-on-write clone of an open file into this directory, or `Unsupported`.
    fn clone_file(&self, src: &File, name: &OsStr) -> io::Result<()>;
}

/// An open file or item handle.
pub trait FileOps: Sized + Send + Sync {
    fn stat(&self) -> io::Result<Entry>;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
    fn set_len(&self, len: u64) -> io::Result<()>;
    fn sync(&self, how: Durability) -> io::Result<()>;
    /// Applies metadata (times last, since the other changes touch them).
    /// Unsupported fields are skipped (the share's caps say which).
    fn apply_meta(&self, m: &SetMeta<'_>) -> io::Result<()>;
}

/// A mounted filesystem holding shares.
pub trait VolumeOps: Sized + Send + Sync {
    fn of(path: &Path) -> io::Result<Self>;
    /// `FileKey::vol` of items on this volume.
    fn key(&self) -> u64;
    /// An identity that survives reboots and remounts (volume UUID).
    fn stable_id(&self) -> String;
    /// Current path of an item, or `Ok(None)` when the OS cannot resolve
    /// keys (the index then walks its own rows).
    fn path_of(&self, key: &FileKey) -> io::Result<Option<PathBuf>>;
}

/// One hint from the watcher. Hints may be coarse or duplicated; the index
/// rescans the indexed directories they touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Something at this path changed: rescan it (if an indexed directory)
    /// and its parent.
    Path(PathBuf),
    /// Rescan every indexed directory under this path.
    Subtree(PathBuf),
    /// Events were lost: rescan every indexed directory.
    All,
}

/// Hints plus, when it is safe to persist, the cursor to resume from.
#[derive(Clone, Debug, Default)]
pub struct WatchBatch {
    pub changes: Vec<Change>,
    pub cursor: Option<String>,
}

/// Change notifications for the share roots.
pub trait WatcherOps: Sized + Send + Sync {
    /// Starts watching. `cursor` is the last one this watcher handed out;
    /// when it can resume, it replays what happened since; when it cannot,
    /// it sends `Change::All` (or, if the index is known current, nothing).
    fn start(roots: &[PathBuf], cursor: Option<&str>, tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self>;
    /// A directory became indexed (non-recursive watchers add a watch).
    fn watch_dir(&self, path: &Path);
    /// A directory left the index.
    fn unwatch_dir(&self, path: &Path);
}

/// The OS trash. Items are moved, never deleted.
pub trait TrashOps: Sized + Send + Sync {
    /// The trash for a user (`home`) and server state dir.
    fn open(home: &Path, data_dir: &Path) -> io::Result<Self>;
    /// Moves `name` out of `dir` into the trash; returns its new path.
    fn trash(&self, dir: &Dir, name: &OsStr) -> io::Result<PathBuf>;
    /// Whether a path is inside this trash.
    fn contains(&self, path: &Path) -> bool;
    /// Moves a trashed item back as `to` in `to_dir` (no replace).
    fn put_back(&self, from: &Path, to_dir: &Dir, to: &OsStr) -> io::Result<()>;
}

/// Thumbnails made on the server.
pub trait ThumbnailerOps {
    const AVAILABLE: bool;
    /// Whether this file type gets a useful picture.
    fn wants(name: &str) -> bool;
    /// A PNG no larger than `px` on its long edge, using `scratch` for
    /// temporary files; `None` when there is none. Must time out and kill
    /// helpers on drop.
    fn generate(path: &Path, px: u32, scratch: &Path) -> impl Future<Output = Option<Vec<u8>>> + Send;
}

/// Process-level facts and conventions.
pub trait SysOps {
    /// The user's home directory.
    fn home() -> Option<PathBuf>;
    /// Server state (certificate, index): macOS `~/Library/Application Support/Homeport/server`.
    fn data_dir(home: &Path) -> PathBuf;
    /// Where `server.toml` lives.
    fn config_dir(home: &Path) -> PathBuf;
    /// Caches (thumbnails).
    fn cache_dir(home: &Path) -> PathBuf;
    /// Shares used when neither the config nor `--share` names any.
    fn default_shares(home: &Path) -> Vec<(String, PathBuf)>;
    /// This machine's name for clients.
    fn host_name() -> String;
    /// Places to look for the Tailscale CLI, in order.
    fn tailscale_candidates() -> Vec<PathBuf>;
    /// A command running the Tailscale CLI non-interactively.
    fn tailscale_command(bin: &Path) -> tokio::process::Command;
    /// Resolves on Ctrl-C or the OS's termination request.
    fn shutdown_signal() -> impl Future<Output = io::Result<()>> + Send;
    /// Local time as `YYYY-MM-DD HHMM` (conflict copy names).
    fn local_stamp() -> String;
    /// Creates a directory only this user can read.
    fn create_private_dir(path: &Path) -> io::Result<()>;
    /// Writes a file only this user can read.
    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()>;
}

/// Running `homeportd` at login / boot.
pub trait ServiceOps {
    /// Installs and starts the service running `exe args...`. With `print`,
    /// only shows what would be written. Returns a report for the user.
    fn install(exe: &Path, args: &[OsString], print: bool) -> io::Result<String>;
    fn uninstall() -> io::Result<String>;
    fn status() -> io::Result<String>;
}

/// Compile-time check that the selected platform implements the contract.
#[allow(dead_code)]
fn conformance() {
    fn fs<T: FsOps>() {}
    fn dir<T: DirOps>() {}
    fn file<T: FileOps>() {}
    fn volume<T: VolumeOps>() {}
    fn watcher<T: WatcherOps>() {}
    fn trash<T: TrashOps>() {}
    fn thumbs<T: ThumbnailerOps>() {}
    fn sys<T: SysOps>() {}
    fn service<T: ServiceOps>() {}
    fs::<Fs>();
    dir::<Dir>();
    file::<File>();
    volume::<Volume>();
    watcher::<Watcher>();
    trash::<Trash>();
    thumbs::<Thumbnailer>();
    sys::<Sys>();
    service::<Service>();
}

/// Copies all of `src` over `dst` (truncated first), in place.
pub fn copy_over(src: &File, dst: &File) -> io::Result<u64> {
    dst.set_len(0)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    loop {
        let n = src.read_at(&mut buf, pos)?;
        if n == 0 {
            return Ok(pos);
        }
        let mut off = 0usize;
        while off < n {
            let w = dst.write_at(&buf[off..n], pos + off as u64)?;
            if w == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            off += w;
        }
        pos += n as u64;
    }
}

/// Whether two open files have the same content.
pub fn same_content(a: &File, b: &File) -> io::Result<bool> {
    if a.stat()?.size != b.stat()?.size {
        return Ok(false);
    }
    let mut x = vec![0u8; 1 << 20];
    let mut y = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    loop {
        let n = a.read_at(&mut x, pos)?;
        let m = b.read_at(&mut y, pos)?;
        if n != m {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
        if x[..n] != y[..n] {
            return Ok(false);
        }
        pos += n as u64;
    }
}
