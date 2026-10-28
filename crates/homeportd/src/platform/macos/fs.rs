//! Filesystem access on macOS: `getattrlistbulk` directory scans, `*at`
//! calls relative to directory descriptors, and `fsgetpath` to turn an APFS
//! file ID back into its current path.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use homeport_proto::{Kind, Os, item_flags};

use crate::platform::unix::{cname, cpath, cvt, kind_from_mode, open_nofollow, timespec, ts_ns};
use crate::platform::{
    DirOps, Durability, Entry, ErrKind, FileKey, FileOps, FsCaps, FsOps, IdScheme, NamePolicy, RawName, SetMeta,
    VolumeOps, copy_over,
};

// Not all of these are exported by the libc crate.
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_CHGTIME: u32 = 0x0000_0800;
const ATTR_CMN_ACCESSMASK: u32 = 0x0002_0000;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const SF_DATALESS: u32 = 0x4000_0000;
const PC_CASE_SENSITIVE: libc::c_int = 11;

unsafe extern "C" {
    fn fsgetpath(buf: *mut libc::c_char, bufsize: usize, fsid: *mut libc::fsid_t, objid: u64) -> isize;
}

/// Finder tags xattr.
pub const TAGS_XATTR: &str = "com.apple.metadata:_kMDItemUserTags";

/// BSD flags → protocol item flags.
fn item_flags(bsd: u32) -> u32 {
    let mut f = bsd & (item_flags::HIDDEN | item_flags::READ_ONLY);
    if bsd & libc::SF_IMMUTABLE != 0 {
        f |= item_flags::READ_ONLY;
    }
    if bsd & SF_DATALESS != 0 {
        f |= item_flags::CLOUD_ONLY;
    }
    f
}

fn entry_from_stat(st: &libc::stat, name: RawName) -> Entry {
    Entry {
        key: FileKey { vol: st.st_dev as u64, id: u128::from(st.st_ino), generation: 0 },
        name,
        kind: kind_from_mode(st.st_mode),
        // Directory sizes are not meaningful; scans report 0 for them too.
        size: if st.st_mode & libc::S_IFMT == libc::S_IFDIR { 0 } else { u64::try_from(st.st_size).unwrap_or(0) },
        mtime_ns: ts_ns(st.st_mtime, st.st_mtime_nsec),
        ctime_ns: ts_ns(st.st_ctime, st.st_ctime_nsec),
        mode: u32::from(st.st_mode) & 0o7777,
        crtime_ns: ts_ns(st.st_birthtime, st.st_birthtime_nsec),
        flags: item_flags(st.st_flags),
    }
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed `stat` is a valid out-parameter; `fd` is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st)
}

fn fsid_raw(f: &libc::fsid_t) -> [i32; 2] {
    // SAFETY: `fsid_t` is `#[repr(C)] struct { val: [i32; 2] }`; its field is private in libc.
    unsafe { std::mem::transmute_copy(f) }
}

/// The macOS filesystem.
pub struct Fs;

impl FsOps for Fs {
    const OS: Os = Os::MacOs;
    const OS_NAME: &'static str = "macOS";
    const ID_SCHEME: IdScheme = IdScheme::Direct;
    const NAME_POLICY: NamePolicy = NamePolicy::Apple;

    fn lstat(path: &Path) -> io::Result<Entry> {
        let c = cpath(path)?;
        // SAFETY: zeroed `stat` is a valid out-parameter; `c` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::lstat(c.as_ptr(), &mut st) })?;
        Ok(entry_from_stat(&st, RawName::of_path(path).unwrap_or_default()))
    }

    fn open_read(path: &Path) -> io::Result<File> {
        Ok(File { fd: open_nofollow(path, libc::O_RDONLY)? })
    }

    fn tags(path: &Path) -> Option<Vec<u8>> {
        get_xattr(path, TAGS_XATTR).ok().flatten()
    }

    fn classify(e: &io::Error) -> ErrKind {
        crate::platform::unix::classify(e)
    }

    fn caps(root: &Path) -> FsCaps {
        let case_sensitive = cpath(root)
            // SAFETY: `c` is NUL-terminated.
            .map(|c| unsafe { libc::pathconf(c.as_ptr(), PC_CASE_SENSITIVE) } == 1)
            .unwrap_or(false);
        FsCaps {
            case_sensitive,
            // APFS and HFS+ compare names normalization-insensitively.
            normalization_sensitive: false,
            symlinks: true,
            xattrs: true,
            tags: true,
            set_crtime: true,
            hidden: true,
            mode: true,
        }
    }

    fn skip(name: &OsStr) -> bool {
        name.as_bytes() == b".DS_Store"
    }

    fn replace_content(dir: &Dir, name: &OsStr, upload: &File, staging: &Dir, meta: &SetMeta<'_>) -> io::Result<Entry> {
        // In place, so the file keeps its ID; a clone guards against a crash mid-copy.
        let target = dir.open_file(name, true)?;
        let st = target.stat()?;
        let backup = format!("backup-{}-{:x}", st.key.id, rand::random::<u32>());
        let backed_up = staging.clone_file(&target, OsStr::new(&backup)).is_ok();
        copy_over(upload, &target)?;
        target.apply_meta(meta)?;
        target.sync(Durability::Full)?;
        if backed_up {
            let _ = staging.unlink(OsStr::new(&backup));
        }
        dir.stat(name)
    }
}

/// The volume shares live on.
#[derive(Clone, Copy)]
pub struct Volume {
    fsid: libc::fsid_t,
    dev: libc::dev_t,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Volume({:?}, dev {})", fsid_raw(&self.fsid), self.dev)
    }
}

impl Volume {
    pub fn dev(&self) -> libc::dev_t {
        self.dev
    }
}

impl VolumeOps for Volume {
    fn of(path: &Path) -> io::Result<Self> {
        let c = cpath(path)?;
        // SAFETY: zeroed out-parameters are valid; `c` is NUL-terminated.
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::statfs(c.as_ptr(), &mut sfs) })?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::lstat(c.as_ptr(), &mut st) })?;
        Ok(Self { fsid: sfs.f_fsid, dev: st.st_dev })
    }

    fn key(&self) -> u64 {
        self.dev as u64
    }

    fn stable_id(&self) -> String {
        super::fsevents::volume_uuid(self.dev).unwrap_or_else(|| format!("dev-{}", self.dev))
    }

    fn path_of(&self, key: &FileKey) -> io::Result<Option<PathBuf>> {
        let ino = u64::try_from(key.id).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        let mut fsid = self.fsid;
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        let n = unsafe { fsgetpath(buf.as_mut_ptr().cast(), buf.len(), &mut fsid, ino) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let cstr = CStr::from_bytes_until_nul(&buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(Some(PathBuf::from(OsStr::from_bytes(cstr.to_bytes()))))
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let end = end.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short attr buffer"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_ne_bytes(a))
    }
    fn timespec(&mut self) -> io::Result<i64> {
        let sec = self.u64()? as i64;
        let nsec = self.u64()? as i64;
        Ok(ts_ns(sec, nsec))
    }
}

/// Lists an open directory with `getattrlistbulk`.
fn scan_fd(fd: &OwnedFd) -> io::Result<Vec<Entry>> {
    let mut al = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_ERROR
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_CRTIME
            | ATTR_CMN_MODTIME
            | ATTR_CMN_CHGTIME
            | ATTR_CMN_ACCESSMASK
            | ATTR_CMN_FLAGS
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut buf = vec![0u8; 256 * 1024];
    let mut out = Vec::new();
    loop {
        // SAFETY: `al` and `buf` are valid for the call; `fd` is an open directory.
        let n = unsafe {
            libc::getattrlistbulk(fd.as_raw_fd(), (&raw mut al).cast(), buf.as_mut_ptr().cast(), buf.len(), 0)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        let mut pos = 0usize;
        for _ in 0..n {
            let len = buf
                .get(pos..pos + 4)
                .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short entry"))?;
            let rec =
                buf.get(pos..pos + len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short entry"))?;
            if let Some(e) = parse_entry(rec)? {
                out.push(e);
            }
            pos += len;
        }
    }
    Ok(out)
}

fn parse_entry(rec: &[u8]) -> io::Result<Option<Entry>> {
    let mut r = Reader { buf: rec, pos: 4 };
    // attribute_set_t: common, vol, dir, file, fork.
    let common = r.u32()?;
    let _vol = r.u32()?;
    let _dir = r.u32()?;
    let file = r.u32()?;
    let _fork = r.u32()?;
    if common & ATTR_CMN_ERROR != 0 && r.u32()? != 0 {
        return Ok(None);
    }
    let mut name = RawName::default();
    if common & ATTR_CMN_NAME != 0 {
        let start = r.pos;
        let off = r.i32()?;
        let len = r.u32()? as usize;
        let at = usize::try_from(start as i64 + i64::from(off))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad name ref"))?;
        let bytes = rec.get(at..at + len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad name ref"))?;
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        name = RawName(OsStr::from_bytes(bytes).to_owned());
    }
    let dev = if common & ATTR_CMN_DEVID != 0 { r.u32()? } else { 0 };
    let mut kind = Kind::Other;
    if common & ATTR_CMN_OBJTYPE != 0 {
        kind = match r.u32()? {
            VREG => Kind::File,
            VDIR => Kind::Dir,
            VLNK => Kind::Symlink,
            _ => Kind::Other,
        };
    }
    let crtime_ns = if common & ATTR_CMN_CRTIME != 0 { r.timespec()? } else { 0 };
    let mtime_ns = if common & ATTR_CMN_MODTIME != 0 { r.timespec()? } else { 0 };
    let ctime_ns = if common & ATTR_CMN_CHGTIME != 0 { r.timespec()? } else { 0 };
    let mode = if common & ATTR_CMN_ACCESSMASK != 0 { r.u32()? & 0o7777 } else { 0 };
    let flags = if common & ATTR_CMN_FLAGS != 0 { r.u32()? } else { 0 };
    let ino = if common & ATTR_CMN_FILEID != 0 { r.u64()? } else { return Ok(None) };
    let size = if file & ATTR_FILE_DATALENGTH != 0 { r.u64()? } else { 0 };
    debug_assert!(r.pos <= rec.len() && size_of::<libc::attribute_set_t>() == 20);
    let key = FileKey { vol: u64::from(dev), id: u128::from(ino), generation: 0 };
    Ok(Some(Entry { key, name, kind, size, mtime_ns, ctime_ns, mode, crtime_ns, flags: item_flags(flags) }))
}

fn xattr_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Reads one extended attribute of `path` (not following a symlink); `None` if absent.
fn get_xattr(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let c = cpath(path)?;
    let n = xattr_name(name)?;
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: `buf` is writable for its length; strings are NUL-terminated.
        let r = unsafe {
            libc::getxattr(c.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW)
        };
        if r >= 0 {
            buf.truncate(r as usize);
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOATTR) => return Ok(None),
            Some(libc::ERANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            _ => return Err(e),
        }
    }
}

/// An open directory.
pub struct Dir {
    fd: OwnedFd,
}

impl Dir {
    fn open_at(&self, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated; the descriptor is ours on success.
        let fd = unsafe { libc::openat(self.fd.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: freshly opened.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Current path of the directory.
    pub(super) fn current_path(&self) -> io::Result<PathBuf> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes into `buf`.
        cvt(unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) })?;
        let cstr = CStr::from_bytes_until_nul(&buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(PathBuf::from(OsStr::from_bytes(cstr.to_bytes())))
    }

    /// `renameatx_np(.., RENAME_EXCL)` to an absolute path. For `~/.Trash`,
    /// which may be renamed into but not opened (privacy protection).
    pub(super) fn rename_to_path_excl(&self, from: &OsStr, to: &Path) -> io::Result<()> {
        let f = cname(from)?;
        let t = cpath(to)?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe {
            libc::renameatx_np(self.fd.as_raw_fd(), f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_EXCL)
        })
    }

    /// `rename_excl` from an absolute path into this directory.
    pub(super) fn rename_from_path_excl(&self, from: &Path, to: &OsStr) -> io::Result<()> {
        let f = cpath(from)?;
        let t = cname(to)?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe {
            libc::renameatx_np(libc::AT_FDCWD, f.as_ptr(), self.fd.as_raw_fd(), t.as_ptr(), libc::RENAME_EXCL)
        })
    }
}

impl DirOps for Dir {
    fn open(path: &Path, expect: Option<&FileKey>) -> io::Result<Self> {
        let fd = open_nofollow(path, libc::O_RDONLY | libc::O_DIRECTORY)?;
        if let Some(k) = expect {
            let st = fstat(&fd)?;
            if u128::from(st.st_ino) != k.id || st.st_dev as u64 != k.vol {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
        }
        Ok(Self { fd })
    }

    fn scan(&self) -> io::Result<Vec<Entry>> {
        scan_fd(&self.fd)
    }

    fn stat(&self, name: &OsStr) -> io::Result<Entry> {
        let c = cname(name)?;
        // SAFETY: zeroed `stat` is a valid out-parameter; `c` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstatat(self.fd.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(entry_from_stat(&st, RawName(name.to_owned())))
    }

    fn stored_name(&self, name: &OsStr) -> io::Result<RawName> {
        let key = self.stat(name)?.key;
        let fd = open_nofollow(&self.current_path()?, libc::O_RDONLY | libc::O_DIRECTORY)?;
        Ok(scan_fd(&fd)?.into_iter().find(|e| e.key.id == key.id).map_or_else(|| RawName(name.to_owned()), |e| e.name))
    }

    fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File> {
        let mode = if write { libc::O_RDWR } else { libc::O_RDONLY };
        Ok(File { fd: self.open_at(name, mode | libc::O_NOFOLLOW)? })
    }

    fn open_meta(&self, name: &OsStr, kind: Kind) -> io::Result<File> {
        let flags = match kind {
            Kind::Dir => libc::O_RDONLY | libc::O_DIRECTORY,
            Kind::Symlink => libc::O_RDONLY | libc::O_SYMLINK,
            _ => libc::O_RDONLY,
        };
        Ok(File { fd: self.open_at(name, flags | libc::O_NOFOLLOW)? })
    }

    fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated.
        cvt(unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o755) })
    }

    fn symlink(&self, target: &str, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        let t = CString::new(target).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe { libc::symlinkat(t.as_ptr(), self.fd.as_raw_fd(), c.as_ptr()) })
    }

    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated.
        cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) })
    }

    /// `renameatx_np(.., RENAME_EXCL)`.
    fn rename_noreplace(&self, from: &OsStr, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        let f = cname(from)?;
        let t = cname(to)?;
        // SAFETY: both strings are NUL-terminated; both descriptors are open directories.
        cvt(unsafe {
            libc::renameatx_np(self.fd.as_raw_fd(), f.as_ptr(), to_dir.fd.as_raw_fd(), t.as_ptr(), libc::RENAME_EXCL)
        })
    }

    fn clone_file(&self, src: &File, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `src` is an open file, `c` is NUL-terminated.
        cvt(unsafe { libc::fclonefileat(src.fd.as_raw_fd(), self.fd.as_raw_fd(), c.as_ptr(), 0) })
    }
}

/// An open file (or any item, for metadata).
pub struct File {
    fd: OwnedFd,
}

/// Extended attributes a client may set: not the system's own bookkeeping.
fn settable_xattr(name: &str) -> bool {
    !name.is_empty()
        && !matches!(
            name,
            "com.apple.provenance" | "com.apple.ResourceFork" | "com.apple.rootless" | "com.apple.decmpfs"
        )
        && !name.starts_with("com.apple.system.")
}

impl FileOps for File {
    fn stat(&self) -> io::Result<Entry> {
        Ok(entry_from_stat(&fstat(&self.fd)?, RawName::default()))
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // SAFETY: `buf` is writable for its length; `fd` is open.
        let n = unsafe { libc::pread(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        // SAFETY: `buf` is readable for its length; `fd` is open.
        let n = unsafe { libc::pwrite(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        // SAFETY: `fd` is open.
        cvt(unsafe { libc::ftruncate(self.fd.as_raw_fd(), len as libc::off_t) })
    }

    fn sync(&self, how: Durability) -> io::Result<()> {
        let cmd = match how {
            Durability::Full => libc::F_FULLFSYNC,
            Durability::Barrier => libc::F_BARRIERFSYNC,
        };
        // SAFETY: `fd` is open.
        if unsafe { libc::fcntl(self.fd.as_raw_fd(), cmd) } < 0 {
            // Not every filesystem supports these; fall back to fsync.
            // SAFETY: `fd` is open.
            cvt(unsafe { libc::fsync(self.fd.as_raw_fd()) })?;
        }
        Ok(())
    }

    fn apply_meta(&self, m: &SetMeta<'_>) -> io::Result<()> {
        let raw = self.fd.as_raw_fd();
        for (k, v) in m.xattrs {
            if !settable_xattr(k) {
                continue;
            }
            let n = xattr_name(k)?;
            // SAFETY: `v` is readable for its length; `n` is NUL-terminated.
            if unsafe { libc::fsetxattr(raw, n.as_ptr(), v.as_ptr().cast(), v.len(), 0, 0) } < 0 {
                tracing::debug!(xattr = k, error = %io::Error::last_os_error(), "fsetxattr");
            }
        }
        if let Some(tags) = m.tags {
            let n = xattr_name(TAGS_XATTR)?;
            match tags {
                // SAFETY: as above.
                Some(t) => cvt(unsafe { libc::fsetxattr(raw, n.as_ptr(), t.as_ptr().cast(), t.len(), 0, 0) })?,
                None => {
                    // SAFETY: `n` is NUL-terminated.
                    if unsafe { libc::fremovexattr(raw, n.as_ptr(), 0) } < 0 {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() != Some(libc::ENOATTR) {
                            return Err(e);
                        }
                    }
                }
            }
        }
        if let Some(mode) = m.mode {
            // SAFETY: `fd` is open.
            cvt(unsafe { libc::fchmod(raw, (mode & 0o7777) as libc::mode_t) })?;
        }
        if let Some(hidden) = m.hidden {
            let flags = fstat(&self.fd)?.st_flags;
            let new = if hidden { flags | libc::UF_HIDDEN } else { flags & !libc::UF_HIDDEN };
            if new != flags {
                // SAFETY: `fd` is open.
                cvt(unsafe { libc::fchflags(raw, new) })?;
            }
        }
        if m.crtime_ns.is_some() || m.mtime_ns.is_some() {
            let mut common = 0u32;
            let mut buf: Vec<libc::timespec> = Vec::new();
            if let Some(c) = m.crtime_ns {
                common |= ATTR_CMN_CRTIME;
                buf.push(timespec(c));
            }
            if let Some(t) = m.mtime_ns {
                common |= ATTR_CMN_MODTIME;
                buf.push(timespec(t));
            }
            let mut al = libc::attrlist {
                bitmapcount: libc::ATTR_BIT_MAP_COUNT,
                reserved: 0,
                commonattr: common,
                volattr: 0,
                dirattr: 0,
                fileattr: 0,
                forkattr: 0,
            };
            // SAFETY: `buf` holds one timespec per requested attribute, in bit order.
            cvt(unsafe {
                libc::fsetattrlist(
                    raw,
                    (&raw mut al).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len() * size_of::<libc::timespec>(),
                    0,
                )
            })?;
        }
        Ok(())
    }
}
