//! Filesystem access: `getattrlistbulk` directory scans, `lstat`, and
//! `fsgetpath` to turn an APFS file ID back into its current path.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use homeport_proto::Kind;

/// Attributes of one directory entry as the filesystem reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ino: u64,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    pub crtime_ns: i64,
    pub flags: u32,
}

// Not all of these are exported by the libc crate.
const ATTR_CMN_NAME: u32 = 0x0000_0001;
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

unsafe extern "C" {
    fn fsgetpath(buf: *mut libc::c_char, bufsize: usize, fsid: *mut libc::fsid_t, objid: u64) -> isize;
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Opens a path without following a final symlink.
pub fn open_nofollow(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    let c = cpath(path)?;
    // SAFETY: `c` is a valid NUL-terminated string.
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor that we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ts_ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

fn kind_from_mode(mode: libc::mode_t) -> Kind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    }
}

fn entry_from_stat(st: &libc::stat, name: String) -> Entry {
    Entry {
        ino: st.st_ino,
        name,
        kind: kind_from_mode(st.st_mode),
        // Directory sizes are not meaningful; scans report 0 for them too.
        size: if st.st_mode & libc::S_IFMT == libc::S_IFDIR { 0 } else { u64::try_from(st.st_size).unwrap_or(0) },
        mtime_ns: ts_ns(st.st_mtime, st.st_mtime_nsec),
        ctime_ns: ts_ns(st.st_ctime, st.st_ctime_nsec),
        mode: u32::from(st.st_mode) & 0o7777,
        crtime_ns: ts_ns(st.st_birthtime, st.st_birthtime_nsec),
        flags: st.st_flags,
    }
}

/// `lstat` a path.
pub fn lstat(path: &Path) -> io::Result<Entry> {
    let c = cpath(path)?;
    // SAFETY: zeroed `stat` is a valid out-parameter; `c` is NUL-terminated.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(entry_from_stat(&st, name))
}

/// `fstat` an open file.
pub fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed `stat` is a valid out-parameter; `fd` is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fsid_raw(f: &libc::fsid_t) -> [i32; 2] {
    // SAFETY: `fsid_t` is `#[repr(C)] struct { val: [i32; 2] }`; its field is private in libc.
    unsafe { std::mem::transmute_copy(f) }
}

/// The volume the shares live on.
#[derive(Clone, Copy)]
pub struct Volume {
    fsid: libc::fsid_t,
    pub dev: libc::dev_t,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Volume({:?}, dev {})", fsid_raw(&self.fsid), self.dev)
    }
}

impl Volume {
    pub fn of(path: &Path) -> io::Result<Self> {
        let c = cpath(path)?;
        // SAFETY: zeroed out-parameters are valid; `c` is NUL-terminated.
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::lstat(c.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fsid: sfs.f_fsid, dev: st.st_dev })
    }

    pub fn same_as(&self, other: &Volume) -> bool {
        fsid_raw(&self.fsid) == fsid_raw(&other.fsid)
    }

    /// Current path of a file ID on this volume.
    pub fn path_of(&self, ino: u64) -> io::Result<PathBuf> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        let mut fsid = self.fsid;
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        let n = unsafe { fsgetpath(buf.as_mut_ptr().cast(), buf.len(), &mut fsid, ino) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let cstr = CStr::from_bytes_until_nul(&buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(PathBuf::from(OsStr::from_bytes(cstr.to_bytes())))
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

/// Lists a directory with `getattrlistbulk`. Never follows a symlink at `dir`.
pub fn scan_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let fd = open_nofollow(dir, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let mut al = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
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
    let mut name = String::new();
    if common & ATTR_CMN_NAME != 0 {
        let start = r.pos;
        let off = r.i32()?;
        let len = r.u32()? as usize;
        let at = usize::try_from(start as i64 + i64::from(off))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad name ref"))?;
        let bytes = rec.get(at..at + len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad name ref"))?;
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        name = String::from_utf8_lossy(bytes).into_owned();
    }
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
    Ok(Some(Entry { ino, name, kind, size, mtime_ns, ctime_ns, mode, crtime_ns, flags }))
}

/// Finder tags xattr.
pub const TAGS_XATTR: &str = "com.apple.metadata:_kMDItemUserTags";

fn xattr_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Reads one extended attribute of `path` (not following a symlink); `None` if absent.
pub fn get_xattr(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
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

fn cvt(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn cname(name: &str) -> io::Result<CString> {
    if !homeport_proto::valid_name(name) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// An open directory; every operation is relative to it and never follows
/// a symlink in the last component.
pub struct Dir {
    pub fd: OwnedFd,
}

impl Dir {
    /// Opens a directory path; with `ino`, fails with `ENOENT` unless it is
    /// still that directory.
    pub fn open(path: &Path, ino: Option<u64>) -> io::Result<Self> {
        let fd = open_nofollow(path, libc::O_RDONLY | libc::O_DIRECTORY)?;
        if let Some(ino) = ino
            && fstat(&fd)?.st_ino != ino
        {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(Self { fd })
    }

    /// `lstat` of an entry.
    pub fn stat(&self, name: &str) -> io::Result<Entry> {
        let c = cname(name)?;
        // SAFETY: zeroed `stat` is a valid out-parameter; `c` is NUL-terminated.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstatat(self.fd.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(entry_from_stat(&st, name.to_owned()))
    }

    /// The entry's name as stored on disk (the volume is case-insensitive).
    pub fn stored_name(&self, name: &str) -> io::Result<String> {
        let ino = self.stat(name)?.ino;
        let path = fd_path(&self.fd)?;
        Ok(scan_dir(&path)?.into_iter().find(|e| e.ino == ino).map_or_else(|| name.to_owned(), |e| e.name))
    }

    pub fn open_at(&self, name: &str, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated; the descriptor is ours on success.
        let fd = unsafe {
            libc::openat(self.fd.as_raw_fd(), c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, libc::c_uint::from(mode))
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: freshly opened.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn mkdir(&self, name: &str, mode: libc::mode_t) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated.
        cvt(unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), mode) })
    }

    pub fn symlink(&self, target: &str, name: &str) -> io::Result<()> {
        let c = cname(name)?;
        let t = CString::new(target).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe { libc::symlinkat(t.as_ptr(), self.fd.as_raw_fd(), c.as_ptr()) })
    }

    pub fn unlink(&self, name: &str) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated.
        cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) })
    }

    /// `renameatx_np(.., RENAME_EXCL)`: fails with `EEXIST` when `to` exists
    /// (except a case-only rename of the same item).
    pub fn rename_excl(&self, from: &str, to_dir: &Dir, to: &str) -> io::Result<()> {
        let f = cname(from)?;
        let t = cname(to)?;
        // SAFETY: both strings are NUL-terminated; both descriptors are open directories.
        cvt(unsafe {
            libc::renameatx_np(self.fd.as_raw_fd(), f.as_ptr(), to_dir.fd.as_raw_fd(), t.as_ptr(), libc::RENAME_EXCL)
        })
    }

    /// `rename_excl` to an absolute path. For `~/.Trash`, which may be
    /// renamed into but not opened (privacy protection).
    pub fn rename_to_path_excl(&self, from: &str, to: &Path) -> io::Result<()> {
        let f = cname(from)?;
        let t = cpath(to)?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe { libc::renameatx_np(self.fd.as_raw_fd(), f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_EXCL) })
    }

    /// `rename_excl` from an absolute path into this directory.
    pub fn rename_from_path_excl(&self, from: &Path, to: &str) -> io::Result<()> {
        let f = cpath(from)?;
        let t = cname(to)?;
        // SAFETY: both strings are NUL-terminated.
        cvt(unsafe { libc::renameatx_np(libc::AT_FDCWD, f.as_ptr(), self.fd.as_raw_fd(), t.as_ptr(), libc::RENAME_EXCL) })
    }

    /// Clones an open file into this directory (copy-on-write).
    pub fn clone_from(&self, src: &OwnedFd, name: &str) -> io::Result<()> {
        let c = cname(name)?;
        // SAFETY: `src` is an open file, `c` is NUL-terminated.
        cvt(unsafe { libc::fclonefileat(src.as_raw_fd(), self.fd.as_raw_fd(), c.as_ptr(), 0) })
    }
}

/// Current path of an open descriptor.
pub fn fd_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes into `buf`.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) })?;
    let cstr = CStr::from_bytes_until_nul(&buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    Ok(PathBuf::from(OsStr::from_bytes(cstr.to_bytes())))
}

/// Durability for a written file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// `F_FULLFSYNC`: on stable storage before we answer.
    Full,
    /// `F_BARRIERFSYNC`: ordered, much cheaper (bulk uploads).
    Barrier,
}

pub fn sync(fd: &OwnedFd, how: Sync) -> io::Result<()> {
    let cmd = match how {
        Sync::Full => libc::F_FULLFSYNC,
        Sync::Barrier => libc::F_BARRIERFSYNC,
    };
    // SAFETY: `fd` is open.
    if unsafe { libc::fcntl(fd.as_raw_fd(), cmd) } < 0 {
        // Not every filesystem supports these; fall back to fsync.
        // SAFETY: `fd` is open.
        cvt(unsafe { libc::fsync(fd.as_raw_fd()) })?;
    }
    Ok(())
}

/// Copies all of `src` over `dst` (which is truncated first), in place.
pub fn copy_over(src: &OwnedFd, dst: &OwnedFd) -> io::Result<u64> {
    // SAFETY: `dst` is open for writing.
    cvt(unsafe { libc::ftruncate(dst.as_raw_fd(), 0) })?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    loop {
        // SAFETY: `buf` is writable for its length.
        let n = unsafe { libc::pread(src.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), pos as libc::off_t) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(pos);
        }
        let mut off = 0usize;
        while off < n as usize {
            // SAFETY: the slice is initialized.
            let w = unsafe {
                libc::pwrite(dst.as_raw_fd(), buf[off..].as_ptr().cast(), n as usize - off, (pos + off as u64) as libc::off_t)
            };
            if w < 0 {
                return Err(io::Error::last_os_error());
            }
            off += w as usize;
        }
        pos += n as u64;
    }
}

/// Whether two open files have the same content.
pub fn same_content(a: &OwnedFd, b: &OwnedFd) -> io::Result<bool> {
    if fstat(a)?.st_size != fstat(b)?.st_size {
        return Ok(false);
    }
    let mut x = vec![0u8; 1 << 20];
    let mut y = vec![0u8; 1 << 20];
    let mut pos: u64 = 0;
    loop {
        // SAFETY: buffers are writable for their length.
        let n = unsafe { libc::pread(a.as_raw_fd(), x.as_mut_ptr().cast(), x.len(), pos as libc::off_t) };
        let m = unsafe { libc::pread(b.as_raw_fd(), y.as_mut_ptr().cast(), y.len(), pos as libc::off_t) };
        if n < 0 || m < 0 {
            return Err(io::Error::last_os_error());
        }
        if n != m {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
        if x[..n as usize] != y[..n as usize] {
            return Ok(false);
        }
        pos += n as u64;
    }
}

/// Metadata to apply to an open item.
pub struct SetMeta<'a> {
    pub mode: Option<u32>,
    pub hidden: Option<bool>,
    pub tags: Option<Option<&'a [u8]>>,
    pub xattrs: &'a [(String, Vec<u8>)],
    pub crtime_ns: Option<i64>,
    pub mtime_ns: Option<i64>,
}

fn timespec(ns: i64) -> libc::timespec {
    libc::timespec { tv_sec: ns.div_euclid(1_000_000_000), tv_nsec: ns.rem_euclid(1_000_000_000) }
}

/// Extended attributes a client may set: not the system's own bookkeeping.
fn settable_xattr(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "com.apple.provenance" | "com.apple.ResourceFork" | "com.apple.rootless" | "com.apple.decmpfs")
        && !name.starts_with("com.apple.system.")
}

/// Applies metadata to an open file or directory. Times go last, since the
/// other changes touch them.
pub fn apply_meta(fd: &OwnedFd, m: &SetMeta<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
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
        let flags = fstat(fd)?.st_flags;
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
