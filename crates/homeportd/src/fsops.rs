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
}

// Not all of these are exported by the libc crate.
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_CHGTIME: u32 = 0x0000_0800;
const ATTR_CMN_ACCESSMASK: u32 = 0x0002_0000;
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
            | ATTR_CMN_MODTIME
            | ATTR_CMN_CHGTIME
            | ATTR_CMN_ACCESSMASK
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
    let mtime_ns = if common & ATTR_CMN_MODTIME != 0 { r.timespec()? } else { 0 };
    let ctime_ns = if common & ATTR_CMN_CHGTIME != 0 { r.timespec()? } else { 0 };
    let mode = if common & ATTR_CMN_ACCESSMASK != 0 { r.u32()? & 0o7777 } else { 0 };
    let ino = if common & ATTR_CMN_FILEID != 0 { r.u64()? } else { return Ok(None) };
    let size = if file & ATTR_FILE_DATALENGTH != 0 { r.u64()? } else { 0 };
    debug_assert!(r.pos <= rec.len() && size_of::<libc::attribute_set_t>() == 20);
    Ok(Some(Entry { ino, name, kind, size, mtime_ns, ctime_ns, mode }))
}
