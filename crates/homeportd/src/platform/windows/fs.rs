//! Filesystem access on Windows (NTFS, ReFS): handle-relative opens with
//! `NtCreateFile(RootDirectory = directory handle)` (no MAX_PATH, no path
//! races), `FileIdExtdDirectoryInfo` listings with 128-bit file ids, and
//! `OpenFileById` + `GetFinalPathNameByHandleW` to turn a key back into a path.
//!
//! Every handle shares read, write and delete and is short-lived, so Explorer,
//! Office and editors are never blocked by us. Sharing violations caused by
//! other processes are retried for about a second before they surface as
//! `ErrKind::Busy`. Cloud placeholders (OneDrive files on demand) are never
//! opened for data: that would download them.

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::Duration;

use homeport_proto::{Kind, Os, item_flags};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_CLOUD_FILE_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_NAME, ERROR_NO_MORE_FILES, ERROR_STOPPED_ON_SYMLINK, HANDLE, INVALID_HANDLE_VALUE,
    OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CreateSymbolicLinkW, DELETE, ExtendedFileIdType, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TAG_INFO, FILE_ATTRIBUTE_TEMPORARY,
    FILE_BASIC_INFO, FILE_CASE_SENSITIVE_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0,
    FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_NAME_INFO,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo, FileBasicInfo,
    FileCaseSensitiveInfo, FileDispositionInfoEx, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo,
    FileNormalizedNameInfo, FileStandardInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING, OpenFileById, ReOpenFile,
    SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY, SYNCHRONIZE,
    SetFileInformationByHandle, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{
    FILE_CS_FLAG_CASE_SENSITIVE_DIR, IO_REPARSE_TAG_APPEXECLINK, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

use crate::platform::{
    DirOps, Durability, Entry, ErrKind, FileKey, FileOps, FsCaps, FsOps, IdScheme, NamePolicy, RawName, SetMeta,
    VolumeOps, classify_kind,
};

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// 100 ns ticks between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_UNABLE_TO_REMOVE_REPLACED: i32 = 1175;
/// Waits between attempts when another process holds a file (~1.1 s in all).
const BUSY_BACKOFF_MS: [u64; 6] = [25, 50, 100, 200, 300, 450];

// --- small helpers -----------------------------------------------------------

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

fn win_err(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn cvt(ok: bool) -> io::Result<()> {
    if ok { Ok(()) } else { Err(last_err()) }
}

fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(last_err());
    }
    // SAFETY: a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
}

fn raw(h: &impl AsRawHandle) -> HANDLE {
    h.as_raw_handle() as HANDLE
}

/// A NUL-terminated wide string.
fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// A path for `CreateFileW` and friends: absolute, `\\?\`-prefixed so long
/// paths and names ending in dots or spaces work, NUL-terminated.
pub(super) fn long_path(path: &Path) -> io::Result<Vec<u16>> {
    let abs = if path.is_absolute() { path.to_owned() } else { std::path::absolute(path)? };
    let w: Vec<u16> =
        abs.as_os_str().encode_wide().map(|c| if c == u16::from(b'/') { u16::from(b'\\') } else { c }).collect();
    let bs = u16::from(b'\\');
    let mut out: Vec<u16> = Vec::with_capacity(w.len() + 8);
    if w.starts_with(&[bs, bs, u16::from(b'?'), bs]) || w.starts_with(&[bs, bs, u16::from(b'.'), bs]) {
        out.extend_from_slice(&w);
    } else if w.starts_with(&[bs, bs]) {
        out.extend("\\\\?\\UNC\\".encode_utf16());
        out.extend_from_slice(&w[2..]);
    } else {
        out.extend("\\\\?\\".encode_utf16());
        out.extend_from_slice(&w);
    }
    out.push(0);
    Ok(out)
}

/// The UTF-16 of a single path component, refusing anything that would make
/// a handle-relative open leave its directory.
fn component(name: &OsStr) -> io::Result<Vec<u16>> {
    let w: Vec<u16> = name.encode_wide().collect();
    let bad = w.is_empty()
        || w == [u16::from(b'.')]
        || w == [u16::from(b'.'), u16::from(b'.')]
        || w.iter().any(|&c| c == 0 || c == u16::from(b'\\') || c == u16::from(b'/'));
    if bad { Err(win_err(ERROR_INVALID_NAME)) } else { Ok(w) }
}

/// Retries `f` while another process holds the file (sharing or lock
/// violation), for about a second.
pub(super) fn busy_retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_UNABLE_TO_REMOVE_REPLACED)
                ) && attempt < BUSY_BACKOFF_MS.len() =>
            {
                std::thread::sleep(Duration::from_millis(BUSY_BACKOFF_MS[attempt]));
                attempt += 1;
            }
            Err(e) if attempt > 0 => {
                if matches!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)) {
                    tracing::info!(error = %e, "file in use by another program; giving up after retries");
                }
                return Err(e);
            }
            r => return r,
        }
    }
}

/// `NtCreateFile` relative to `root` (a directory handle; or the file itself
/// with an empty `name`).
fn nt_open(root: HANDLE, name: &[u16], access: u32, disposition: u32, options: u32) -> io::Result<OwnedHandle> {
    let bytes = u16::try_from(name.len() * 2).map_err(|_| win_err(ERROR_INVALID_NAME))?;
    let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr().cast_mut() };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &us,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut h: HANDLE = null_mut();
    // SAFETY: zeroed IO_STATUS_BLOCK is a valid out-parameter.
    let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is valid for the call; `us` borrows `name`.
    let status = unsafe {
        NtCreateFile(
            &mut h,
            access | SYNCHRONIZE,
            &oa,
            &mut iosb,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            SHARE_ALL,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: no preconditions.
        return Err(win_err(unsafe { RtlNtStatusToDosError(status) }));
    }
    owned(h)
}

/// `CreateFileW` of an absolute path, never following a final reparse point.
pub(super) fn open_path(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let p = long_path(path)?;
    // SAFETY: `p` is NUL-terminated.
    owned(unsafe {
        CreateFileW(
            p.as_ptr(),
            access,
            SHARE_ALL,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    })
}

/// Reopens a handle to the same file with other access rights.
fn reopen(h: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    // SAFETY: `h` is an open handle.
    owned(unsafe { ReOpenFile(h, access, SHARE_ALL, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT) })
}

fn get_info<T: Copy>(h: HANDLE, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    // SAFETY: `v` is writable for `size_of::<T>()` bytes.
    cvt(unsafe { GetFileInformationByHandleEx(h, class, v.as_mut_ptr().cast(), size_of::<T>() as u32) } != 0)?;
    // SAFETY: initialized by the call (and zeroed before).
    Ok(unsafe { v.assume_init() })
}

fn set_info<T>(h: HANDLE, class: FILE_INFO_BY_HANDLE_CLASS, v: &T, len: usize) -> io::Result<()> {
    // SAFETY: `v` is readable for `len` bytes.
    cvt(unsafe { SetFileInformationByHandle(h, class, (v as *const T).cast::<c_void>(), len as u32) } != 0)
}

/// Current path of an open item (`\\?\C:\...`, on-disk case).
pub(super) fn final_path(h: HANDLE) -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` is writable for its length.
        let n = unsafe {
            GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS)
        } as usize;
        if n == 0 {
            return Err(last_err());
        }
        if n < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

pub(super) fn ft_ns(t: i64) -> i64 {
    if t == 0 { 0 } else { (t - EPOCH_DIFF).saturating_mul(100) }
}

fn ns_ft(ns: i64) -> i64 {
    ns.div_euclid(100) + EPOCH_DIFF
}

fn id_of(f: &FILE_ID_128) -> u128 {
    u128::from_le_bytes(f.Identifier)
}

/// Reparse tags that point elsewhere: shown as symlinks, never traversed.
fn is_link_tag(tag: u32) -> bool {
    tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT
}

fn kind_of(attrs: u32, tag: u32) -> Kind {
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        if is_link_tag(tag) {
            return Kind::Symlink;
        }
        // AppExecLinks and other name surrogates (WSL symlinks, ...).
        if tag == IO_REPARSE_TAG_APPEXECLINK || tag & 0x2000_0000 != 0 {
            return Kind::Other;
        }
    }
    // Dedup, WOF/compressed and cloud tags are ordinary files and folders.
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 { Kind::Dir } else { Kind::File }
}

/// Content not on this disk: reading it would download it.
fn is_cloud_only(attrs: u32) -> bool {
    attrs & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE) != 0
}

fn flags_of(attrs: u32) -> u32 {
    let mut f = 0;
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        f |= item_flags::HIDDEN;
    }
    if attrs & FILE_ATTRIBUTE_READONLY != 0 && attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        f |= item_flags::READ_ONLY;
    }
    if is_cloud_only(attrs) {
        f |= item_flags::CLOUD_ONLY;
    }
    f
}

fn mode_of(kind: Kind, attrs: u32) -> u32 {
    let m = match kind {
        Kind::Dir => 0o755,
        Kind::Symlink => 0o777,
        _ => 0o644,
    };
    if kind == Kind::File && attrs & FILE_ATTRIBUTE_READONLY != 0 { m & !0o222 } else { m }
}

#[allow(clippy::too_many_arguments)]
fn make_entry(vol: u64, id: u128, name: RawName, attrs: u32, tag: u32, size: i64, times: (i64, i64, i64)) -> Entry {
    let kind = kind_of(attrs, tag);
    Entry {
        key: FileKey { vol, id, generation: 0 },
        name,
        kind,
        size: if kind == Kind::Dir { 0 } else { u64::try_from(size).unwrap_or(0) },
        crtime_ns: ft_ns(times.0),
        mtime_ns: ft_ns(times.1),
        ctime_ns: ft_ns(times.2),
        mode: mode_of(kind, attrs),
        flags: flags_of(attrs),
    }
}

/// Attributes of an open item, from the handle (not the lazily updated
/// directory entry).
fn entry_of_handle(h: HANDLE, name: RawName) -> io::Result<Entry> {
    let basic: FILE_BASIC_INFO = get_info(h, FileBasicInfo)?;
    let std_info: FILE_STANDARD_INFO = get_info(h, FileStandardInfo)?;
    let id: FILE_ID_INFO = get_info(h, FileIdInfo)?;
    let tag = if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        get_info::<FILE_ATTRIBUTE_TAG_INFO>(h, FileAttributeTagInfo)?.ReparseTag
    } else {
        0
    };
    Ok(make_entry(
        id.VolumeSerialNumber,
        id_of(&id.FileId),
        name,
        basic.FileAttributes,
        tag,
        std_info.EndOfFile,
        (basic.CreationTime, basic.LastWriteTime, basic.ChangeTime),
    ))
}

/// Refuses links and placeholders before an item is opened for data.
fn check_data_open(h: HANDLE) -> io::Result<()> {
    let basic: FILE_BASIC_INFO = get_info(h, FileBasicInfo)?;
    let attrs = basic.FileAttributes;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        let tag = get_info::<FILE_ATTRIBUTE_TAG_INFO>(h, FileAttributeTagInfo)?.ReparseTag;
        if is_link_tag(tag) || tag & 0x2000_0000 != 0 {
            return Err(win_err(ERROR_STOPPED_ON_SYMLINK));
        }
    }
    if is_cloud_only(attrs) {
        return Err(win_err(ERROR_CLOUD_FILE_ACCESS_DENIED));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(win_err(ERROR_ACCESS_DENIED));
    }
    Ok(())
}

/// Renames the item behind `h` to `name` in the directory `to_dir`, with
/// `NtSetInformationFile(FileRenameInformationEx)`: kernelbase's
/// `SetFileInformationByHandle` refuses a `RootDirectory`.
fn rename_handle(h: HANDLE, to_dir: HANDLE, name: &[u16], replace: bool) -> io::Result<()> {
    let name_off = offset_of!(FILE_RENAME_INFO, FileName);
    let len = name_off + name.len() * 2;
    // u64s keep the buffer aligned for the struct.
    let mut buf = vec![0u64; len.div_ceil(8) + 1];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let mut flags = FILE_RENAME_FLAG_POSIX_SEMANTICS;
    if replace {
        flags |= FILE_RENAME_FLAG_REPLACE_IF_EXISTS;
    }
    // SAFETY: `buf` is large enough for the header and the name.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = to_dir;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(name_off).cast::<u16>(),
            name.len(),
        );
    }
    busy_retry(|| {
        // SAFETY: zeroed IO_STATUS_BLOCK is a valid out-parameter.
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: `buf` holds a FILE_RENAME_INFORMATION of `len` bytes.
        let status =
            unsafe { NtSetInformationFile(h, &mut iosb, buf.as_ptr().cast(), len as u32, FileRenameInformationEx) };
        if status < 0 {
            // SAFETY: no preconditions.
            return Err(win_err(unsafe { RtlNtStatusToDosError(status) }));
        }
        Ok(())
    })
}

/// Whether Developer Mode allows unprivileged symlinks.
fn developer_mode() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    let key = wide_nul(OsStr::new("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\AppModelUnlock"));
    let val = wide_nul(OsStr::new("AllowDevelopmentWithoutDevLicense"));
    let mut data: u32 = 0;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: strings are NUL-terminated; `data` is writable for `len` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&raw mut data).cast(),
            &mut len,
        )
    };
    r == 0 && data == 1
}

// --- Fs ----------------------------------------------------------------------

/// Path operations.
pub struct Fs;

impl FsOps for Fs {
    const OS: Os = Os::Windows;
    const OS_NAME: &'static str = "Windows";
    const ID_SCHEME: IdScheme = IdScheme::Assigned;
    const NAME_POLICY: NamePolicy = NamePolicy::Windows;

    fn lstat(path: &Path) -> io::Result<Entry> {
        let h = open_path(path, FILE_READ_ATTRIBUTES)?;
        entry_of_handle(raw(&h), RawName::of_path(path).unwrap_or_default())
    }

    fn open_read(path: &Path) -> io::Result<File> {
        let h = open_path(path, FILE_READ_ATTRIBUTES)?;
        check_data_open(raw(&h))?;
        let data = busy_retry(|| reopen(raw(&h), GENERIC_READ | FILE_READ_ATTRIBUTES))?;
        Ok(File::new(data))
    }

    /// No Finder tags on Windows (no xattrs; not mirrored to an ADS).
    fn tags(_path: &Path) -> Option<Vec<u8>> {
        None
    }

    fn classify(e: &io::Error) -> ErrKind {
        match e.raw_os_error() {
            Some(2 | 3 | 1168) => ErrKind::NotFound,
            Some(5 | 19 | 1314) => ErrKind::Denied,
            Some(32 | 33 | 1175) => ErrKind::Busy,
            Some(80 | 183) => ErrKind::Exists,
            Some(145) => ErrKind::NotEmpty,
            Some(39 | 112) => ErrKind::NoSpace,
            Some(123 | 161 | 206) => ErrKind::BadName,
            Some(17) => ErrKind::CrossDevice,
            Some(267) => ErrKind::NotADirectory,
            Some(681 | 1921 | 4390) => ErrKind::Symlink,
            Some(362..=404 | 1920) => ErrKind::CloudOnly,
            Some(50 | 120) => ErrKind::Unsupported,
            Some(_) => ErrKind::Other,
            None => classify_kind(e),
        }
    }

    fn caps(root: &Path) -> FsCaps {
        let case_sensitive = open_path(root, FILE_READ_ATTRIBUTES)
            .and_then(|h| get_info::<FILE_CASE_SENSITIVE_INFO>(raw(&h), FileCaseSensitiveInfo))
            .is_ok_and(|i| i.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0);
        FsCaps {
            case_sensitive,
            // NTFS compares UTF-16 code units: NFC and NFD names can coexist.
            normalization_sensitive: true,
            symlinks: developer_mode(),
            xattrs: false,
            tags: false,
            set_crtime: true,
            hidden: true,
            mode: false,
        }
    }

    fn skip(name: &OsStr) -> bool {
        let n = name.to_string_lossy();
        ["desktop.ini", "thumbs.db", "$recycle.bin", "system volume information"]
            .iter()
            .any(|s| n.eq_ignore_ascii_case(s))
    }

    /// The upload replaces the target with a POSIX-semantics rename
    /// (`FileRenameInformationEx`, `REPLACE_IF_EXISTS | POSIX_SEMANTICS`):
    /// atomic, and it works while other programs have the target open with
    /// delete sharing (editors, Explorer previews). The target's attributes
    /// and creation time are copied first. The file gets the upload's FileId;
    /// the index carries the item id over. (`ReplaceFileW` would also keep
    /// the ACL and alternate streams, but it cannot run while the caller
    /// holds the upload open.) A read-only target is refused, as on macOS.
    fn replace_content(
        dir: &Dir,
        name: &OsStr,
        upload: &File,
        _staging: &Dir,
        meta: &SetMeta<'_>,
    ) -> io::Result<Entry> {
        let w = component(name)?;
        let cur_h = dir.open_rel(name, FILE_READ_ATTRIBUTES, 0)?;
        let cur: FILE_BASIC_INFO = get_info(raw(&cur_h), FileBasicInfo)?;
        drop(cur_h);
        let attrs = cur.FileAttributes;
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(win_err(ERROR_ACCESS_DENIED));
        }
        if is_cloud_only(attrs) {
            return Err(win_err(ERROR_CLOUD_FILE_ACCESS_DENIED));
        }
        if attrs & FILE_ATTRIBUTE_READONLY != 0 {
            return Err(win_err(ERROR_ACCESS_DENIED));
        }
        upload.sync(Durability::Full)?;
        let h = busy_retry(|| reopen(raw(&upload.f), DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES))?;
        let keep =
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
        let carry = FILE_BASIC_INFO {
            CreationTime: cur.CreationTime,
            LastAccessTime: 0,
            LastWriteTime: 0,
            ChangeTime: 0,
            FileAttributes: if attrs & keep == 0 { FILE_ATTRIBUTE_NORMAL } else { attrs & keep },
        };
        set_info(raw(&h), FileBasicInfo, &carry, size_of::<FILE_BASIC_INFO>())?;
        let f = File::new(h);
        f.apply_meta(meta)?;
        rename_handle(raw(&f.f), raw(&dir.h), &w, true)?;
        drop(f);
        dir.stat(name)
    }
}

// --- Volume ------------------------------------------------------------------

/// The volume a share lives on.
pub struct Volume {
    /// Any handle on the volume: the hint for `OpenFileById`.
    root: OwnedHandle,
    serial: u64,
    guid: String,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Volume({:016x}, {})", self.serial, self.guid)
    }
}

/// `\\?\Volume{GUID}\` of the volume holding `path`.
pub(super) fn volume_guid(path: &Path) -> Option<String> {
    let p = wide_nul(path.as_os_str());
    let mut mount = vec![0u16; 1024];
    // SAFETY: `p` is NUL-terminated; `mount` is writable for its length.
    if unsafe { GetVolumePathNameW(p.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0 {
        return None;
    }
    let mut name = vec![0u16; 64];
    // SAFETY: `mount` is NUL-terminated; `name` is writable for its length.
    if unsafe { GetVolumeNameForVolumeMountPointW(mount.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 {
        return None;
    }
    let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..n]))
}

impl Volume {
    /// Opens an item of this volume by key.
    pub(super) fn open_key(hint: HANDLE, key: &FileKey, access: u32) -> io::Result<OwnedHandle> {
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { ExtendedFileId: FILE_ID_128 { Identifier: key.id.to_le_bytes() } },
        };
        // SAFETY: `desc` is valid; `hint` is an open handle.
        let h = unsafe {
            OpenFileById(
                hint,
                &desc,
                access,
                SHARE_ALL,
                null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        owned(h).map_err(|e| match e.raw_os_error() {
            // A stale id: ERROR_INVALID_PARAMETER, or the record was reused.
            Some(87 | 2 | 3 | 1168) => win_err(ERROR_FILE_NOT_FOUND),
            _ => e,
        })
    }
}

impl VolumeOps for Volume {
    fn of(path: &Path) -> io::Result<Self> {
        let root = open_path(path, FILE_READ_ATTRIBUTES)?;
        let id: FILE_ID_INFO = get_info(raw(&root), FileIdInfo)?;
        let serial = id.VolumeSerialNumber;
        let guid = volume_guid(path).unwrap_or_else(|| format!("serial-{serial:016x}"));
        Ok(Self { root, serial, guid })
    }

    fn key(&self) -> u64 {
        self.serial
    }

    fn stable_id(&self) -> String {
        self.guid.clone()
    }

    fn path_of(&self, key: &FileKey) -> io::Result<Option<PathBuf>> {
        if key.vol != self.serial {
            return Err(win_err(ERROR_FILE_NOT_FOUND));
        }
        let h = Self::open_key(raw(&self.root), key, FILE_READ_ATTRIBUTES)?;
        Ok(Some(final_path(raw(&h))?))
    }
}

// --- Dir ---------------------------------------------------------------------

/// An open directory.
pub struct Dir {
    h: OwnedHandle,
    vol: u64,
    /// Listing state lives in the handle: one scan at a time.
    scan_lock: Mutex<()>,
}

const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;

impl Dir {
    fn from_handle(h: OwnedHandle, expect: Option<&FileKey>) -> io::Result<Self> {
        let basic: FILE_BASIC_INFO = get_info(raw(&h), FileBasicInfo)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let tag = get_info::<FILE_ATTRIBUTE_TAG_INFO>(raw(&h), FileAttributeTagInfo)?.ReparseTag;
            if is_link_tag(tag) || tag & 0x2000_0000 != 0 {
                return Err(win_err(ERROR_STOPPED_ON_SYMLINK));
            }
        }
        if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(win_err(ERROR_DIRECTORY));
        }
        let id: FILE_ID_INFO = get_info(raw(&h), FileIdInfo)?;
        if let Some(k) = expect
            && (id_of(&id.FileId) != k.id || id.VolumeSerialNumber != k.vol)
        {
            return Err(win_err(ERROR_FILE_NOT_FOUND));
        }
        Ok(Self { h, vol: id.VolumeSerialNumber, scan_lock: Mutex::new(()) })
    }

    pub(super) fn handle(&self) -> HANDLE {
        raw(&self.h)
    }

    /// Current path of the directory.
    pub(super) fn path(&self) -> io::Result<PathBuf> {
        final_path(raw(&self.h))
    }

    fn open_rel(&self, name: &OsStr, access: u32, options: u32) -> io::Result<OwnedHandle> {
        let w = component(name)?;
        busy_retry(|| nt_open(raw(&self.h), &w, access, FILE_OPEN, options | FILE_OPEN_REPARSE_POINT))
    }

    /// Opens a child (any kind) by name; for the trash and put back.
    pub(super) fn open_child(&self, name: &OsStr, access: u32) -> io::Result<OwnedHandle> {
        self.open_rel(name, access, 0)
    }

    /// Moves the item behind `h` here as `name` (no replace).
    pub(super) fn rename_into(&self, h: HANDLE, name: &OsStr) -> io::Result<()> {
        rename_handle(h, raw(&self.h), &component(name)?, false)
    }
}

impl DirOps for Dir {
    fn open(path: &Path, expect: Option<&FileKey>) -> io::Result<Self> {
        Self::from_handle(open_path(path, DIR_ACCESS)?, expect)
    }

    fn scan(&self) -> io::Result<Vec<Entry>> {
        let _g = self.scan_lock.lock().unwrap_or_else(|p| p.into_inner());
        let h = raw(&self.h);
        // u64s keep entries 8-byte aligned as the API requires.
        let mut buf = vec![0u64; 256 * 1024 / 8];
        let mut out = Vec::new();
        let mut class = FileIdExtdDirectoryRestartInfo;
        let name_off = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        loop {
            // SAFETY: `buf` is writable for its byte length.
            let ok = unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
            if ok == 0 {
                let e = last_err();
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    break;
                }
                return Err(e);
            }
            class = FileIdExtdDirectoryInfo;
            let bytes = buf.as_ptr().cast::<u8>();
            let total = buf.len() * 8;
            let mut off = 0usize;
            loop {
                if off + name_off > total {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short directory entry"));
                }
                // SAFETY: bounds checked above; entries are 8-byte aligned.
                let info = unsafe { &*bytes.add(off).cast::<FILE_ID_EXTD_DIR_INFO>() };
                let nlen = info.FileNameLength as usize / 2;
                if off + name_off + nlen * 2 > total {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short directory entry"));
                }
                // SAFETY: bounds checked above.
                let name = unsafe { std::slice::from_raw_parts(bytes.add(off + name_off).cast::<u16>(), nlen) };
                let attrs = info.FileAttributes;
                let dots = name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')];
                let hidden_system = attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)
                    == FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
                if !dots && !hidden_system {
                    out.push(make_entry(
                        self.vol,
                        id_of(&info.FileId),
                        RawName(OsString::from_wide(name)),
                        attrs,
                        info.ReparsePointTag,
                        info.EndOfFile,
                        (info.CreationTime, info.LastWriteTime, info.ChangeTime),
                    ));
                }
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
            }
        }
        Ok(out)
    }

    fn stat(&self, name: &OsStr) -> io::Result<Entry> {
        let h = self.open_rel(name, FILE_READ_ATTRIBUTES, 0)?;
        entry_of_handle(raw(&h), RawName(name.to_owned()))
    }

    fn stored_name(&self, name: &OsStr) -> io::Result<RawName> {
        let h = self.open_rel(name, FILE_READ_ATTRIBUTES, 0)?;
        let mut buf = vec![0u64; (size_of::<FILE_NAME_INFO>() + 2 * 32_768).div_ceil(8)];
        // SAFETY: `buf` is writable for its byte length.
        cvt(unsafe {
            GetFileInformationByHandleEx(
                raw(&h),
                FileNormalizedNameInfo,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
            )
        } != 0)?;
        // SAFETY: filled by the call.
        let info = unsafe { &*buf.as_ptr().cast::<FILE_NAME_INFO>() };
        let n = info.FileNameLength as usize / 2;
        let off = offset_of!(FILE_NAME_INFO, FileName);
        // SAFETY: the name follows the header within `buf`.
        let path = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>().add(off).cast::<u16>(), n) };
        let last = path.rsplit(|&c| c == u16::from(b'\\')).next().unwrap_or(path);
        if last.is_empty() { Ok(RawName(name.to_owned())) } else { Ok(RawName(OsString::from_wide(last))) }
    }

    fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File> {
        let attrs = self.open_rel(name, FILE_READ_ATTRIBUTES, 0)?;
        check_data_open(raw(&attrs))?;
        let access = if write { GENERIC_READ | GENERIC_WRITE } else { GENERIC_READ | FILE_READ_ATTRIBUTES };
        let data = busy_retry(|| reopen(raw(&attrs), access))?;
        Ok(File::new(data))
    }

    fn open_meta(&self, name: &OsStr, _kind: Kind) -> io::Result<File> {
        Ok(File::new(self.open_rel(name, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, 0)?))
    }

    fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        let w = component(name)?;
        nt_open(raw(&self.h), &w, FILE_LIST_DIRECTORY, FILE_CREATE, FILE_DIRECTORY_FILE).map(drop)
    }

    /// Needs Developer Mode (or `SeCreateSymbolicLinkPrivilege`); the share's
    /// caps say whether clients may try.
    fn symlink(&self, target: &str, name: &OsStr) -> io::Result<()> {
        component(name)?;
        let dir = self.path()?;
        let target_w = target.replace('/', "\\");
        let resolved = if Path::new(&target_w).is_absolute() { PathBuf::from(&target_w) } else { dir.join(&target_w) };
        let mut flags = SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE;
        if std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir()) {
            flags |= SYMBOLIC_LINK_FLAG_DIRECTORY;
        }
        let link = long_path(&dir.join(name))?;
        let t = wide_nul(OsStr::new(&target_w));
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe { CreateSymbolicLinkW(link.as_ptr(), t.as_ptr(), flags) })
    }

    /// POSIX-semantics delete. Only used for our own staging files.
    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let h = self.open_rel(name, DELETE | FILE_READ_ATTRIBUTES, FILE_NON_DIRECTORY_FILE)?;
        let info = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        set_info(raw(&h), FileDispositionInfoEx, &info, size_of::<FILE_DISPOSITION_INFO_EX>())
    }

    fn rename_noreplace(&self, from: &OsStr, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        let to_w = component(to)?;
        let h = self.open_rel(from, DELETE | FILE_READ_ATTRIBUTES, 0)?;
        match rename_handle(raw(&h), raw(&to_dir.h), &to_w, false) {
            Err(e) if e.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => {
                // A case-only rename of the same item is fine.
                let same = to_dir
                    .stat(to)
                    .is_ok_and(|t| entry_of_handle(raw(&h), RawName::default()).is_ok_and(|s| s.key == t.key));
                if same { rename_handle(raw(&h), raw(&to_dir.h), &to_w, true) } else { Err(e) }
            }
            r => r,
        }
    }

    /// Block cloning is ReFS-only; NTFS shares (all Windows 10 desktops) copy.
    fn clone_file(&self, _src: &File, _name: &OsStr) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no copy-on-write clones on NTFS"))
    }
}

// --- File --------------------------------------------------------------------

/// An open file (or any item, for metadata).
pub struct File {
    f: std::fs::File,
}

impl File {
    fn new(h: OwnedHandle) -> Self {
        Self { f: std::fs::File::from(h) }
    }

    /// Runs `op` on this handle, or on one reopened with `access` when this
    /// one lacks the rights (a read-only handle asked to set metadata).
    fn with_access<T>(&self, access: u32, op: impl Fn(HANDLE) -> io::Result<T>) -> io::Result<T> {
        match op(raw(&self.f)) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                let h = busy_retry(|| reopen(raw(&self.f), access))?;
                op(raw(&h))
            }
            r => r,
        }
    }
}

impl FileOps for File {
    fn stat(&self) -> io::Result<Entry> {
        entry_of_handle(raw(&self.f), RawName::default())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.f.seek_read(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.f.seek_write(buf, offset)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.f.set_len(len)
    }

    /// `FlushFileBuffers` (needs write access: a read-only handle is reopened;
    /// a read-only file was flushed by whoever wrote it).
    fn sync(&self, _how: Durability) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
        // SAFETY: an open handle.
        let flush = |h: HANDLE| cvt(unsafe { FlushFileBuffers(h) } != 0);
        match self.with_access(GENERIC_WRITE, flush) {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                tracing::debug!("cannot flush a read-only file");
                Ok(())
            }
            r => r,
        }
    }

    /// Times, hidden and read-only (from the user-write bit) through
    /// `FileBasicInfo`. Tags and xattrs are not supported (the caps say so).
    fn apply_meta(&self, m: &SetMeta<'_>) -> io::Result<()> {
        if m.mode.is_none() && m.hidden.is_none() && m.crtime_ns.is_none() && m.mtime_ns.is_none() {
            return Ok(());
        }
        self.with_access(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, |h| {
            let cur: FILE_BASIC_INFO = get_info(h, FileBasicInfo)?;
            let mut attrs = cur.FileAttributes;
            if let Some(hidden) = m.hidden {
                attrs = if hidden { attrs | FILE_ATTRIBUTE_HIDDEN } else { attrs & !FILE_ATTRIBUTE_HIDDEN };
            }
            if let Some(mode) = m.mode
                && attrs & FILE_ATTRIBUTE_DIRECTORY == 0
            {
                attrs =
                    if mode & 0o200 == 0 { attrs | FILE_ATTRIBUTE_READONLY } else { attrs & !FILE_ATTRIBUTE_READONLY };
            }
            let settable = FILE_ATTRIBUTE_READONLY
                | FILE_ATTRIBUTE_HIDDEN
                | FILE_ATTRIBUTE_SYSTEM
                | FILE_ATTRIBUTE_ARCHIVE
                | FILE_ATTRIBUTE_TEMPORARY
                | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
            let new_attrs = if attrs == cur.FileAttributes {
                0
            } else if attrs & settable == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attrs & settable
            };
            let info = FILE_BASIC_INFO {
                CreationTime: m.crtime_ns.map_or(0, ns_ft),
                LastAccessTime: 0,
                LastWriteTime: m.mtime_ns.map_or(0, ns_ft),
                ChangeTime: 0,
                FileAttributes: new_attrs,
            };
            set_info(h, FileBasicInfo, &info, size_of::<FILE_BASIC_INFO>())
        })
    }
}
