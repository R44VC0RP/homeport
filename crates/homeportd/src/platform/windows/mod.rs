//! Windows 10+ (NTFS, ReFS). **Stub**: every filesystem function returns
//! [`unsupported`]. Process plumbing (`Sys`) is implemented.
//!
//! Contract for the implementation (see the porting brief, §1–§11). Shared
//! code already handles, for every platform:
//! - item ids: `IdScheme::Assigned` makes the index assign ids and keep
//!   `FileKey → id` in its `ids` table, with the continuity rule for
//!   replace-by-rename saves (Office, JetBrains) and `replace_content`;
//! - id → path: `Volume::path_of` (`OpenFileById` + `GetFinalPathNameByHandleW`)
//!   is tried first; when it returns `Ok(None)` the index walks its rows;
//! - names: `RawName` stores UTF-16; display names map the SFM private-use
//!   characters back to `" * : < > ? \ |`, escape unpaired surrogates, and
//!   `RawName::from_display` maps the other way, NFC-normalizes and refuses
//!   reserved device names (`names.rs`, `NamePolicy::Windows`); case
//!   collisions get aliases when a directory is case-sensitive;
//! - item flags: set HIDDEN, READ_ONLY and CLOUD_ONLY in `Entry::flags`.
//!
//! Crates: `windows-sys` (Win32_Storage_FileSystem, Win32_System_IO,
//! Win32_System_Ioctl, Win32_UI_Shell, Win32_System_Com, Win32_Security,
//! Win32_Graphics_Gdi). Add under `[target.'cfg(windows)'.dependencies]`.
//! Always open with share `READ|WRITE|DELETE` and keep handles short-lived so
//! Explorer and Office are never blocked; map `ERROR_SHARING_VIOLATION` (32)
//! and `ERROR_LOCK_VIOLATION` (33) to `ErrKind::Busy` after 3–5 retries over ~1 s.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use homeport_proto::{Kind, Os};
use tokio::sync::mpsc;

use super::{
    DirOps, Durability, Entry, ErrKind, FileKey, FileOps, FsCaps, FsOps, IdScheme, NamePolicy, RawName, ServiceOps,
    SetMeta, SysOps, ThumbnailerOps, TrashOps, VolumeOps, WatchBatch, WatcherOps, classify_kind, unsupported,
};

/// Path operations.
pub struct Fs;

impl FsOps for Fs {
    const OS: Os = Os::Windows;
    const OS_NAME: &'static str = "Windows";
    const ID_SCHEME: IdScheme = IdScheme::Assigned;
    const NAME_POLICY: NamePolicy = NamePolicy::Windows;

    /// `CreateFileW(FILE_READ_ATTRIBUTES, share R|W|D,
    /// FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT)` +
    /// `GetFileInformationByHandleEx(FileBasicInfo, FileStandardInfo,
    /// FileIdInfo, FileAttributeTagInfo)`. Key: volume serial + FILE_ID_128.
    /// Times: `(t - 116444736000000000) * 100` ns. Kind: reparse tags
    /// `IO_REPARSE_TAG_SYMLINK`/`MOUNT_POINT` → `Symlink`, AppExecLink →
    /// `Other`, other tags (dedup, WOF, cloud) → by directory bit. Mode:
    /// synthesize `0o644`/`0o755`, `0o444` with READONLY. Never open cloud
    /// placeholders for data.
    fn lstat(_path: &Path) -> io::Result<Entry> {
        Err(unsupported("lstat"))
    }

    /// `CreateFileW(GENERIC_READ, share R|W|D, FILE_FLAG_OPEN_REPARSE_POINT)`;
    /// reparse point → `ErrKind::Symlink`; `RECALL_ON_DATA_ACCESS`/`OFFLINE`
    /// → `ErrKind::CloudOnly` (check attributes before opening for data).
    fn open_read(_path: &Path) -> io::Result<File> {
        Err(unsupported("open_read"))
    }

    /// Finder tags live in the index DB on Windows (no xattrs); optionally
    /// mirrored to the `AFP_AfpInfo` ADS like the macOS SMB client. `None`
    /// until then.
    fn tags(_path: &Path) -> Option<Vec<u8>> {
        None
    }

    /// Win32 error codes: 2/3 NotFound, 5 Denied, 32/33 Busy, 80/183 Exists,
    /// 145 NotEmpty, 112 NoSpace, 123/206 BadName, 17 CrossDevice, 267
    /// NotADirectory, 1920/4390 (cloud) CloudOnly.
    fn classify(e: &io::Error) -> ErrKind {
        match e.raw_os_error() {
            Some(2 | 3) => ErrKind::NotFound,
            Some(5) => ErrKind::Denied,
            Some(32 | 33) => ErrKind::Busy,
            Some(80 | 183) => ErrKind::Exists,
            Some(145) => ErrKind::NotEmpty,
            Some(112) => ErrKind::NoSpace,
            Some(123 | 206) => ErrKind::BadName,
            Some(17) => ErrKind::CrossDevice,
            Some(267) => ErrKind::NotADirectory,
            Some(_) => ErrKind::Other,
            None => classify_kind(e),
        }
    }

    /// NTFS: case-insensitive (but `FileCaseSensitiveInfo` per directory,
    /// WSL), normalization-sensitive (NFC and NFD names can coexist), no
    /// xattrs, `set_crtime` and `hidden` true, `symlinks` only with Developer
    /// Mode / `SeCreateSymbolicLinkPrivilege` (probe), `mode` false.
    fn caps(_root: &Path) -> FsCaps {
        FsCaps {
            case_sensitive: false,
            normalization_sensitive: true,
            set_crtime: true,
            hidden: true,
            ..FsCaps::default()
        }
    }

    /// `desktop.ini`, `Thumbs.db`, `$RECYCLE.BIN`, `System Volume Information`;
    /// also drop entries with both HIDDEN and SYSTEM in `scan`.
    fn skip(name: &OsStr) -> bool {
        let n = name.to_string_lossy();
        ["desktop.ini", "thumbs.db", "$recycle.bin", "system volume information"]
            .iter()
            .any(|s| n.eq_ignore_ascii_case(s))
    }

    /// Write the content to a temp file in the target's directory, then
    /// `ReplaceFileW` (keeps the target's ACLs, ADS, creation time,
    /// attributes) or `FileRenameInfoEx` with `REPLACE_IF_EXISTS |
    /// POSIX_SEMANTICS`; `FlushFileBuffers`. Retry sharing violations. The
    /// index keeps the item id for the new FileId.
    fn replace_content(
        _dir: &Dir,
        _name: &OsStr,
        _upload: &File,
        _staging: &Dir,
        _meta: &SetMeta<'_>,
    ) -> io::Result<Entry> {
        Err(unsupported("replace_content"))
    }
}

/// An open directory handle (`FILE_LIST_DIRECTORY | FILE_TRAVERSE`,
/// `FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT`). Relative opens
/// with `NtCreateFile(RootDirectory = handle)` avoid MAX_PATH and races.
pub struct Dir;

impl DirOps for Dir {
    /// Open the directory handle; with `expect`, compare `FileIdInfo`.
    fn open(_path: &Path, _expect: Option<&FileKey>) -> io::Result<Self> {
        Err(unsupported("Dir::open"))
    }

    /// Loop `GetFileInformationByHandleEx(FileIdExtdDirectoryRestartInfo,
    /// then FileIdExtdDirectoryInfo)` with a 64 KiB–1 MiB buffer:
    /// `FILE_ID_EXTD_DIR_INFO` has the 128-bit id, times, size, attributes and
    /// reparse tag without opening files (sizes of files being written are
    /// stale; fine for listings). Flags: HIDDEN, READ_ONLY, CLOUD_ONLY
    /// (`RECALL_ON_DATA_ACCESS` 0x400000, `RECALL_ON_OPEN` 0x40000, `OFFLINE`,
    /// `IO_REPARSE_TAG_CLOUD*`). Skip `.`/`..` and HIDDEN|SYSTEM entries.
    fn scan(&self) -> io::Result<Vec<Entry>> {
        Err(unsupported("scan"))
    }

    /// Handle-based attributes (not the lazily updated directory entry).
    fn stat(&self, _name: &OsStr) -> io::Result<Entry> {
        Err(unsupported("stat"))
    }

    /// `GetFileInformationByHandleEx(FileNameInfo)` on the opened entry, or
    /// the matching entry from `scan` (NTFS preserves case).
    fn stored_name(&self, _name: &OsStr) -> io::Result<RawName> {
        Err(unsupported("stored_name"))
    }

    fn open_file(&self, _name: &OsStr, _write: bool) -> io::Result<File> {
        Err(unsupported("open_file"))
    }

    /// `FILE_WRITE_ATTRIBUTES | READ_CONTROL`, `FILE_FLAG_OPEN_REPARSE_POINT`
    /// (and `BACKUP_SEMANTICS` for directories).
    fn open_meta(&self, _name: &OsStr, _kind: Kind) -> io::Result<File> {
        Err(unsupported("open_meta"))
    }

    /// `CreateDirectoryW` (or `NtCreateFile(FILE_CREATE, FILE_DIRECTORY_FILE)`).
    fn mkdir(&self, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("mkdir"))
    }

    /// `CreateSymbolicLinkW(.., SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE
    /// [| DIRECTORY])`; decide file vs directory from the target if it exists.
    fn symlink(&self, _target: &str, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("symlink"))
    }

    /// `FileDispositionInfoEx` with `POSIX_SEMANTICS` (staging cleanup only).
    fn unlink(&self, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("unlink"))
    }

    /// `SetFileInformationByHandle(FileRenameInfoEx)` without
    /// `REPLACE_IF_EXISTS` (`ERROR_ALREADY_EXISTS` → `Exists`). A case-only
    /// rename of the same file must succeed.
    fn rename_noreplace(&self, _from: &OsStr, _to_dir: &Dir, _to: &OsStr) -> io::Result<()> {
        Err(unsupported("rename"))
    }

    /// ReFS: `FSCTL_DUPLICATE_EXTENTS_TO_FILE`; NTFS: `Unsupported`.
    fn clone_file(&self, _src: &File, _name: &OsStr) -> io::Result<()> {
        Err(unsupported("clone"))
    }
}

/// An open file handle.
pub struct File;

impl FileOps for File {
    /// `GetFileInformationByHandleEx(FileBasicInfo, FileStandardInfo, FileIdInfo)`.
    fn stat(&self) -> io::Result<Entry> {
        Err(unsupported("File::stat"))
    }

    /// `ReadFile` with an `OVERLAPPED` offset (`std::os::windows::fs::FileExt::seek_read`).
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<usize> {
        Err(unsupported("read"))
    }

    /// `seek_write`.
    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        Err(unsupported("write"))
    }

    /// `SetFileInformationByHandle(FileEndOfFileInfo)`.
    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(unsupported("truncate"))
    }

    /// `FlushFileBuffers` for both.
    fn sync(&self, _how: Durability) -> io::Result<()> {
        Err(unsupported("sync"))
    }

    /// times (creation, last write) + hidden + read-only via
    /// `SetFileInformationByHandle(FileBasicInfo)`; mode: only the user-write
    /// bit maps to READONLY (remember an exec bit in the DB later); xattrs and
    /// tags: DB (optionally ADS) — skip for now.
    fn apply_meta(&self, _m: &SetMeta<'_>) -> io::Result<()> {
        Err(unsupported("apply_meta"))
    }
}

/// A volume (`GetVolumeInformationByHandleW`).
pub struct Volume;

impl VolumeOps for Volume {
    fn of(_path: &Path) -> io::Result<Self> {
        Err(unsupported("serving files"))
    }

    /// Volume serial number.
    fn key(&self) -> u64 {
        0
    }

    /// Volume GUID path (`GetVolumeNameForVolumeMountPointW`).
    fn stable_id(&self) -> String {
        String::new()
    }

    /// `OpenFileById(share root handle, ExtendedFileIdType, FILE_READ_ATTRIBUTES)`,
    /// then `GetFinalPathNameByHandleW(FILE_NAME_NORMALIZED)`, strip `\\?\`.
    /// Hard links return one arbitrary path. `Ok(None)` makes the index walk.
    fn path_of(&self, _key: &FileKey) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }
}

/// `ReadDirectoryChangesExW` per share root (+ USN catch-up).
pub struct Watcher;

impl WatcherOps for Watcher {
    /// One thread per share root: `ReadDirectoryChangesExW(bWatchSubtree,
    /// ReadDirectoryNotifyExtendedInformation)` with a 1 MiB buffer; events →
    /// `Change::Path(full path)`, rename OLD/NEW pairs → both paths; overflow
    /// (`ERROR_NOTIFY_ENUM_DIR` or zero bytes) → `Change::All`; stop with
    /// `CancelIoEx`. Cursor: `<JournalId>:<USN>` of the volume via
    /// `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` when available — replay FRNs as
    /// `Change::Path` of their resolved parents; without one, or when the
    /// journal wrapped (`FirstUsn` > cursor, JournalId changed), send
    /// `Change::All`.
    fn start(_roots: &[PathBuf], _cursor: Option<&str>, _tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        Err(unsupported("watching"))
    }

    /// Recursive per root: nothing to do.
    fn watch_dir(&self, _path: &Path) {}

    fn unwatch_dir(&self, _path: &Path) {}
}

/// The Recycle Bin.
pub struct Trash;

impl TrashOps for Trash {
    /// homeportd must run as the user (the bin is per SID).
    fn open(_home: &Path, _data_dir: &Path) -> io::Result<Self> {
        Err(unsupported("trash"))
    }

    /// `IFileOperation` on a COM STA thread with `FOF_ALLOWUNDO | FOF_NO_UI |
    /// FOFX_RECYCLEONDELETE`; `IFileOperationProgressSink::PostDeleteItem`
    /// gives the new item in `$Recycle.Bin\<SID>\$R…` (return its path).
    /// Never permanently delete: refuse when the item exceeds the volume's
    /// `BitBucket` `MaxCapacity` or `NukeOnDelete=1`.
    fn trash(&self, _dir: &Dir, _name: &OsStr) -> io::Result<PathBuf> {
        Err(unsupported("trash"))
    }

    /// Under `<drive>\$Recycle.Bin\<SID>\` (case-insensitive compare).
    fn contains(&self, _path: &Path) -> bool {
        false
    }

    /// Rename `from` back (no replace) and delete its `$I…` info file.
    fn put_back(&self, _from: &Path, _to_dir: &Dir, _to: &OsStr) -> io::Result<()> {
        Err(unsupported("put back"))
    }
}

/// Explorer's thumbnails.
pub struct Thumbnailer;

impl ThumbnailerOps for Thumbnailer {
    const AVAILABLE: bool = false;

    fn wants(_name: &str) -> bool {
        false
    }

    /// `SHCreateItemFromParsingName` → `IShellItemImageFactory::GetImage(
    /// SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK)` on a COM STA thread →
    /// HBITMAP → PNG. `SIIGBF_INCACHEONLY` for cloud placeholders (never
    /// hydrate). Fallback: the `image` crate for jpg/png/webp/gif.
    async fn generate(_path: &Path, _px: u32, _scratch: &Path) -> Option<Vec<u8>> {
        None
    }
}

/// Process plumbing (implemented).
pub struct Sys;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

/// `YYYY-MM-DD HHMM` from Unix seconds (UTC).
fn civil(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}{:02}", rem / 3600, rem % 3600 / 60)
}

impl SysOps for Sys {
    fn home() -> Option<PathBuf> {
        env_dir("USERPROFILE")
    }

    /// `%LOCALAPPDATA%\Homeport\server`.
    fn data_dir(home: &Path) -> PathBuf {
        env_dir("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData\\Local")).join("Homeport\\server")
    }

    /// `%APPDATA%\Homeport`.
    fn config_dir(home: &Path) -> PathBuf {
        env_dir("APPDATA").unwrap_or_else(|| home.join("AppData\\Roaming")).join("Homeport")
    }

    /// `%LOCALAPPDATA%\Homeport\cache`.
    fn cache_dir(home: &Path) -> PathBuf {
        env_dir("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData\\Local")).join("Homeport\\cache")
    }

    /// None: shares come from `homeportd share add`.
    fn default_shares(_home: &Path) -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    fn host_name() -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "homeport".into())
    }

    fn tailscale_candidates() -> Vec<PathBuf> {
        let pf = env_dir("ProgramFiles").unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        vec![pf.join("Tailscale\\tailscale.exe")]
    }

    /// `CREATE_NO_WINDOW` (0x08000000) keeps a console from flashing when run
    /// as a scheduled task.
    fn tailscale_command(bin: &Path) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(bin);
        c.creation_flags(0x0800_0000);
        c
    }

    /// Ctrl-C, or the console/service shutdown (`tokio::signal::windows::ctrl_shutdown`).
    async fn shutdown_signal() -> io::Result<()> {
        let mut shutdown = tokio::signal::windows::ctrl_shutdown()?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = shutdown.recv() => Ok(()),
        }
    }

    /// UTC for now; use `GetLocalTime` once `windows-sys` is a dependency.
    fn local_stamp() -> String {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        civil(secs as i64)
    }

    /// `%LOCALAPPDATA%` is only readable by the user; ACLs inherit.
    fn create_private_dir(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
}

/// Scheduled task (S4U).
pub struct Service;

impl ServiceOps for Service {
    /// Register a scheduled task "Homeport server" (Task Scheduler COM API or
    /// `schtasks /Create /XML`): trigger at startup, run as the current user
    /// with `LogonType=S4U` ("run whether user is logged on or not", "do not
    /// store password"), restart on failure every minute, no execution time
    /// limit, `exe args`. Add a firewall rule:
    /// `New-NetFirewallRule -Direction Inbound -Protocol UDP -LocalPort <port>
    /// -RemoteAddress 100.64.0.0/10,fd7a:115c:a1e0::/48 -Program <exe>
    /// -Profile Any` (needs elevation: print the command if not elevated).
    /// Remind that Tailscale must run unattended. With `print`, show the XML.
    fn install(_exe: &Path, _args: &[OsString], _print: bool) -> io::Result<String> {
        Err(unsupported("service install"))
    }

    /// Delete the task and the firewall rule.
    fn uninstall() -> io::Result<String> {
        Err(unsupported("service uninstall"))
    }

    /// Task state, last run result, and whether the process is running.
    fn status() -> io::Result<String> {
        Err(unsupported("service status"))
    }
}
