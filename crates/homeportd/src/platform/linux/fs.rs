//! Filesystem access on Linux: `getdents64` + `statx` directory scans, `*at`
//! calls relative to directory descriptors, and `openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS)` so that a path inside a share can never be steered
//! out of it by a symlink in any component.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use homeport_proto::{Kind, Os};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, ResolveFlags, Statx, StatxFlags, XattrFlags};

use super::tags;
use crate::platform::unix::{cname, kind_from_mode, ts_ns};
use crate::platform::{
    DirOps, Durability, Entry, ErrKind, FileKey, FileOps, FsCaps, FsOps, IdScheme, NamePolicy, RawName, SetMeta,
    VolumeOps, copy_over,
};

/// Finder tags, stored verbatim as a `user.` xattr (binary plist).
pub const TAGS_XATTR: &str = "user.com.apple.metadata:_kMDItemUserTags";
/// Tags as KDE/Baloo read them: a comma-separated list.
pub const XDG_TAGS_XATTR: &str = "user.xdg.tags";
/// Prefix of `replace_content` temp files (hidden from listings).
const TMP_PREFIX: &str = ".homeport-tmp-";

const MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::INO)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::CTIME)
    .union(StatxFlags::BTIME)
    .union(StatxFlags::MNT_ID);

/// Entries per directory above which `scan` stats on several threads.
const PARALLEL_SCAN: usize = 1024;
const SCAN_THREADS: usize = 4;

/// Share roots seen by [`Fs::caps`]; paths under them are opened confined.
static ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(super) fn err(e: rustix::io::Errno) -> io::Error {
    io::Error::from(e)
}

fn ns(t: &rustix::fs::StatxTimestamp) -> i64 {
    ts_ns(t.tv_sec, i64::from(t.tv_nsec))
}

fn entry_from_statx(st: &Statx, name: RawName) -> Entry {
    let mode = u32::from(st.stx_mode);
    let kind = kind_from_mode(mode);
    let has = |f: StatxFlags| st.stx_mask & f.bits() != 0;
    let crtime_ns = if has(StatxFlags::BTIME) { ns(&st.stx_btime) } else { 0 };
    Entry {
        key: FileKey {
            vol: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
            id: u128::from(st.stx_ino),
            // A reused inode gets a new birth time.
            generation: u64::try_from(crtime_ns).unwrap_or(0),
        },
        name,
        kind,
        size: if kind == Kind::Dir { 0 } else { st.stx_size },
        mtime_ns: ns(&st.stx_mtime),
        ctime_ns: ns(&st.stx_ctime),
        crtime_ns,
        mode: mode & 0o7777,
        flags: 0,
    }
}

fn statx_at(dirfd: impl AsFd, name: &CStr) -> io::Result<Statx> {
    rustix::fs::statx(dirfd, name, AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC, MASK).map_err(err)
}

fn statx_fd(fd: impl AsFd) -> io::Result<Statx> {
    rustix::fs::statx(fd, c"", AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW, MASK).map_err(err)
}

fn mnt_id(st: &Statx) -> u64 {
    if st.stx_mask & StatxFlags::MNT_ID.bits() != 0 { st.stx_mnt_id } else { 0 }
}

fn cstr(b: &[u8]) -> io::Result<CString> {
    CString::new(b).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// Opens `path` without following a symlink in any component inside a share
/// (`openat2` beneath the share root), or in the last component elsewhere.
pub(super) fn open_confined(path: &Path, flags: OFlags) -> io::Result<OwnedFd> {
    let flags = flags | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let root = {
        let roots = ROOTS.lock().unwrap_or_else(|p| p.into_inner());
        roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()).cloned()
    };
    if let Some(root) = root {
        let rel = path.strip_prefix(&root).unwrap_or(Path::new(""));
        let rel = if rel.as_os_str().is_empty() { Path::new(".") } else { rel };
        let rootfd =
            rustix::fs::open(&root, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).map_err(err)?;
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        let open = || rustix::fs::openat2(&rootfd, rel, flags, Mode::empty(), resolve);
        let mut r = open();
        // EAGAIN: a concurrent rename somewhere on the system; try again.
        for _ in 0..3 {
            if !matches!(r, Err(rustix::io::Errno::AGAIN)) {
                break;
            }
            r = open();
        }
        match r {
            // Linux < 5.6: confine the last component only (below).
            Err(rustix::io::Errno::NOSYS) => {}
            r => return r.map_err(err),
        }
    }
    rustix::fs::open(path, flags, Mode::empty()).map_err(err)
}

fn get_xattr_path(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; 4096];
    loop {
        match rustix::fs::lgetxattr(path, name, &mut buf[..]) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(rustix::io::Errno::NODATA) => return Ok(None),
            Err(rustix::io::Errno::RANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            Err(e) => return Err(err(e)),
        }
    }
}

fn get_xattr_fd(fd: impl AsFd, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    let fd = fd.as_fd();
    let mut buf = vec![0u8; 4096];
    loop {
        match rustix::fs::fgetxattr(fd, name, &mut buf[..]) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(rustix::io::Errno::NODATA) => return Ok(None),
            Err(rustix::io::Errno::RANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            Err(e) => return Err(err(e)),
        }
    }
}

/// `user.*` xattr names of an open file.
fn list_user_xattrs(fd: impl AsFd) -> Vec<CString> {
    let fd = fd.as_fd();
    let mut buf = vec![0u8; 4096];
    let n = loop {
        match rustix::fs::flistxattr(fd, &mut buf[..]) {
            Ok(n) => break n,
            Err(rustix::io::Errno::RANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            Err(_) => return Vec::new(),
        }
    };
    buf[..n].split(|&b| b == 0).filter(|n| n.starts_with(b"user.")).filter_map(|n| CString::new(n).ok()).collect()
}

/// Copies every byte of `src` to `dst` (truncated first): `copy_file_range`
/// in the kernel, else read/write.
fn copy_all(src: &File, dst: &File) -> io::Result<()> {
    rustix::fs::ftruncate(&dst.fd, 0).map_err(err)?;
    let size = src.stat()?.size;
    let (mut off_in, mut off_out) = (0u64, 0u64);
    while off_in < size {
        let want = usize::try_from(size - off_in).unwrap_or(usize::MAX).min(1 << 30);
        match rustix::fs::copy_file_range(&src.fd, Some(&mut off_in), &dst.fd, Some(&mut off_out), want) {
            Ok(0) => break,
            Ok(_) => {}
            Err(rustix::io::Errno::XDEV | rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) if off_in == 0 => {
                return copy_over(src, dst).map(|_| ());
            }
            Err(e) => return Err(err(e)),
        }
    }
    Ok(())
}

/// The Linux filesystem.
pub struct Fs;

impl Fs {
    fn probe(root: &Path) -> io::Result<FsCaps> {
        let dir_name = format!(".homeport-probe-{:x}", rand::random::<u64>());
        let parent = Dir::open(root, None)?;
        parent.mkdir(OsStr::new(&dir_name))?;
        let res = (|| {
            let d = Dir::open(&root.join(&dir_name), None)?;
            let touch = |n: &[u8]| -> io::Result<()> {
                let c = cstr(n)?;
                rustix::fs::openat(
                    &d.fd,
                    &c,
                    OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )
                .map(drop)
                .map_err(err)
            };
            let exists = |n: &[u8]| cstr(n).map(|c| statx_at(&d.fd, &c).is_ok()).unwrap_or(false);
            touch(b"a")?;
            let case_sensitive = !exists(b"A");
            touch("\u{e9}".as_bytes())?;
            let normalization_sensitive = !exists("e\u{301}".as_bytes());
            let xattrs = rustix::fs::fsetxattr(&d.fd, "user.homeport.probe", b"1", XattrFlags::empty()).is_ok();
            Ok(FsCaps {
                case_sensitive,
                normalization_sensitive,
                symlinks: true,
                xattrs,
                tags: xattrs,
                set_crtime: false,
                hidden: false,
                mode: true,
            })
        })();
        // Best effort: remove whatever the probe made.
        if let Ok(d) = Dir::open(&root.join(&dir_name), None)
            && let Ok(names) = d.names()
        {
            for (_, n) in names {
                let _ = rustix::fs::unlinkat(&d.fd, &n, AtFlags::empty());
            }
        }
        let _ = parent.rmdir(OsStr::new(&dir_name));
        res
    }
}

impl FsOps for Fs {
    const OS: Os = Os::Linux;
    const OS_NAME: &'static str = "Linux";
    const ID_SCHEME: IdScheme = IdScheme::Assigned;
    const NAME_POLICY: NamePolicy = NamePolicy::Posix;

    fn lstat(path: &Path) -> io::Result<Entry> {
        let st = rustix::fs::statx(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC, MASK)
            .map_err(err)?;
        Ok(entry_from_statx(&st, RawName::of_path(path).unwrap_or_default()))
    }

    /// Non-blocking so a FIFO in a share can't hang the open; regular files
    /// ignore the flag.
    fn open_read(path: &Path) -> io::Result<File> {
        Ok(File::plain(open_confined(path, OFlags::RDONLY | OFlags::NONBLOCK)?))
    }

    fn tags(path: &Path) -> Option<Vec<u8>> {
        if let Ok(Some(t)) = get_xattr_path(path, TAGS_XATTR) {
            return Some(t);
        }
        let xdg = get_xattr_path(path, XDG_TAGS_XATTR).ok().flatten()?;
        tags::plist_from_xdg(&xdg)
    }

    fn classify(e: &io::Error) -> ErrKind {
        crate::platform::unix::classify(e)
    }

    /// Probes the root like Mutagen: makes `a` and `é` (NFC) in a temp dir
    /// and checks whether `A` and `é` (NFD) find them. Also registers the root
    /// so paths under it are opened confined to it.
    fn caps(root: &Path) -> FsCaps {
        {
            let mut roots = ROOTS.lock().unwrap_or_else(|p| p.into_inner());
            if !roots.iter().any(|r| r == root) {
                roots.push(root.to_owned());
            }
        }
        match Self::probe(root) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "could not probe the share's filesystem; assuming ext4");
                FsCaps {
                    case_sensitive: true,
                    normalization_sensitive: true,
                    symlinks: true,
                    mode: true,
                    ..FsCaps::default()
                }
            }
        }
    }

    /// `.Trash-<uid>` / `.Trash` at filesystem roots, `lost+found`, and our
    /// own temp files.
    fn skip(name: &OsStr) -> bool {
        let b = name.as_bytes();
        b == b"lost+found" || b == b".Trash" || b.starts_with(b".Trash-") || b.starts_with(TMP_PREFIX.as_bytes())
    }

    /// A temp file in the same directory (mode and `user.*` xattrs copied
    /// from the target), the new content, `apply_meta`, `fsync`, `renameat`
    /// over the target, `fsync` of the directory. The item gets a new inode;
    /// the index keeps its id.
    fn replace_content(
        dir: &Dir,
        name: &OsStr,
        upload: &File,
        _staging: &Dir,
        meta: &SetMeta<'_>,
    ) -> io::Result<Entry> {
        let target = dir.open_file(name, false)?;
        let st = target.stat()?;
        let tmp_name = OsString::from(format!("{TMP_PREFIX}{:016x}", rand::random::<u64>()));
        let tmp_c = cname(&tmp_name)?;
        let fd = rustix::fs::openat(
            &dir.fd,
            &tmp_c,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(err)?;
        let tmp = File::plain(fd);
        let res = (|| {
            rustix::fs::fchmod(&tmp.fd, Mode::from_raw_mode(st.mode & 0o7777)).map_err(err)?;
            for n in list_user_xattrs(&target.fd) {
                if let Ok(Some(v)) = get_xattr_fd(&target.fd, &n) {
                    let _ = rustix::fs::fsetxattr(&tmp.fd, &n, &v, XattrFlags::empty());
                }
            }
            copy_all(upload, &tmp)?;
            tmp.apply_meta(meta)?;
            tmp.sync(Durability::Full)?;
            let to = cname(name)?;
            rustix::fs::renameat(&dir.fd, &tmp_c, &dir.fd, &to).map_err(err)
        })();
        if let Err(e) = res {
            let _ = rustix::fs::unlinkat(&dir.fd, &tmp_c, AtFlags::empty());
            return Err(e);
        }
        rustix::fs::fsync(&dir.fd).map_err(err)?;
        dir.stat(name)
    }
}

/// A mounted filesystem.
#[derive(Clone, Debug)]
pub struct Volume {
    dev: u64,
    major: u32,
    minor: u32,
    mnt_id: u64,
    path: PathBuf,
}

/// One line of `/proc/self/mountinfo`: (mount id, major:minor, mount point).
fn mountinfo() -> Vec<(u64, String, PathBuf)> {
    let Ok(text) = std::fs::read("/proc/self/mountinfo") else { return Vec::new() };
    text.split(|&b| b == b'\n')
        .filter_map(|l| {
            let mut f = l.split(|&b| b == b' ');
            let id = std::str::from_utf8(f.next()?).ok()?.parse().ok()?;
            let _parent = f.next()?;
            let dev = String::from_utf8_lossy(f.next()?).into_owned();
            let _root = f.next()?;
            let mp = unescape_mount(f.next()?);
            Some((id, dev, PathBuf::from(OsString::from_vec(mp))))
        })
        .collect()
}

/// Undoes mountinfo's octal escapes (`\040` for space).
fn unescape_mount(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Mount point of the filesystem holding an item with this statx.
pub(super) fn mount_point_of(st: &Statx) -> Option<PathBuf> {
    let infos = mountinfo();
    let id = mnt_id(st);
    if id != 0
        && let Some((_, _, mp)) = infos.iter().find(|(i, _, _)| *i == id)
    {
        return Some(mp.clone());
    }
    let dev = format!("{}:{}", st.stx_dev_major, st.stx_dev_minor);
    infos.iter().rev().find(|(_, d, _)| *d == dev).map(|(_, _, mp)| mp.clone())
}

impl VolumeOps for Volume {
    fn of(path: &Path) -> io::Result<Self> {
        let st = rustix::fs::statx(rustix::fs::CWD, path, AtFlags::STATX_DONT_SYNC, MASK).map_err(err)?;
        Ok(Self {
            dev: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
            major: st.stx_dev_major,
            minor: st.stx_dev_minor,
            mnt_id: mnt_id(&st),
            path: path.to_owned(),
        })
    }

    fn key(&self) -> u64 {
        self.dev
    }

    /// The filesystem UUID from `/dev/disk/by-uuid` (a block device with our
    /// major:minor), else `statfs.f_fsid` (btrfs, zfs, tmpfs), else the dev.
    fn stable_id(&self) -> String {
        if let Ok(rd) = std::fs::read_dir("/dev/disk/by-uuid") {
            for e in rd.flatten() {
                if let Ok(st) = rustix::fs::stat(e.path())
                    && rustix::fs::major(st.st_rdev) == self.major
                    && rustix::fs::minor(st.st_rdev) == self.minor
                {
                    return format!("uuid-{}", e.file_name().to_string_lossy());
                }
            }
        }
        if let Ok(sfs) = rustix::fs::statfs(&self.path) {
            // SAFETY: `fsid_t` is two i32s; its field is private in libc.
            let raw: [i32; 2] = unsafe { std::mem::transmute_copy(&sfs.f_fsid) };
            if raw != [0, 0] {
                return format!("fsid-{:08x}{:08x}", raw[0] as u32, raw[1] as u32);
            }
        }
        format!("dev-{}:{}-mnt{}", self.major, self.minor, self.mnt_id)
    }

    /// No unprivileged id → path call (`open_by_handle_at` needs
    /// `CAP_DAC_READ_SEARCH`): the index walks its own rows.
    fn path_of(&self, _key: &FileKey) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }
}

/// An open directory (`O_RDONLY | O_DIRECTORY`).
pub struct Dir {
    pub(super) fd: OwnedFd,
    /// Mount id of the directory (0 if the kernel doesn't report it).
    mnt_id: u64,
}

impl Dir {
    /// Names (and inode numbers) of every entry, via `getdents64` on a fresh
    /// descriptor so concurrent scans don't share an offset.
    fn names(&self) -> io::Result<Vec<(u64, CString)>> {
        let fd =
            rustix::fs::openat(&self.fd, c".", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
                .map_err(err)?;
        let mut buf = vec![MaybeUninit::<u8>::uninit(); 128 * 1024];
        let mut iter = rustix::fs::RawDir::new(&fd, &mut buf);
        let mut out = Vec::new();
        while let Some(e) = iter.next() {
            let e = e.map_err(err)?;
            let n = e.file_name();
            if n == c"." || n == c".." {
                continue;
            }
            out.push((e.ino(), n.to_owned()));
        }
        Ok(out)
    }

    fn entry_of(&self, name: &CStr) -> Option<io::Result<Entry>> {
        match statx_at(&self.fd, name) {
            Ok(st) => {
                let mut e = entry_from_statx(&st, RawName(OsStr::from_bytes(name.to_bytes()).to_owned()));
                if self.mnt_id != 0 && mnt_id(&st) != 0 && mnt_id(&st) != self.mnt_id {
                    // A mount point inside the share: never descend.
                    e.kind = Kind::Other;
                    e.size = 0;
                }
                Some(Ok(e))
            }
            // Deleted between getdents and statx.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
            Err(e) => Some(Err(e)),
        }
    }

    fn rmdir(&self, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        rustix::fs::unlinkat(&self.fd, &c, AtFlags::REMOVEDIR).map_err(err)
    }

    /// Current path of the directory (`/proc/self/fd`).
    pub(super) fn current_path(&self) -> io::Result<PathBuf> {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", self.fd.as_raw_fd()))
    }

    pub(super) fn statx(&self) -> io::Result<Statx> {
        statx_fd(&self.fd)
    }

    fn from_fd(fd: OwnedFd) -> io::Result<(Self, Statx)> {
        let st = statx_fd(&fd)?;
        Ok((Self { fd, mnt_id: mnt_id(&st) }, st))
    }
}

impl DirOps for Dir {
    fn open(path: &Path, expect: Option<&FileKey>) -> io::Result<Self> {
        let fd = open_confined(path, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let (dir, st) = Self::from_fd(fd)?;
        if let Some(k) = expect
            && !entry_from_statx(&st, RawName::default()).key.same_file(k)
        {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(dir)
    }

    fn scan(&self) -> io::Result<Vec<Entry>> {
        let mut names = self.names()?;
        // Inode order reads inode tables sequentially on ext4/xfs.
        names.sort_unstable_by_key(|(ino, _)| *ino);
        if names.len() < PARALLEL_SCAN {
            return names.iter().filter_map(|(_, n)| self.entry_of(n)).collect();
        }
        let chunk = names.len().div_ceil(SCAN_THREADS);
        let parts: Vec<io::Result<Vec<Entry>>> = std::thread::scope(|s| {
            let handles: Vec<_> = names
                .chunks(chunk)
                .map(|c| s.spawn(move || c.iter().filter_map(|(_, n)| self.entry_of(n)).collect()))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("scan thread panicked"))))
                .collect()
        });
        let mut out = Vec::with_capacity(names.len());
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    fn stat(&self, name: &OsStr) -> io::Result<Entry> {
        let c = cname(name)?;
        self.entry_of(&c).unwrap_or_else(|| Err(io::Error::from_raw_os_error(libc::ENOENT)))
    }

    /// The name as given when an entry has exactly those bytes (always on
    /// case-sensitive filesystems); on a casefold directory, the entry with
    /// the same inode.
    fn stored_name(&self, name: &OsStr) -> io::Result<RawName> {
        let key = self.stat(name)?.key;
        let names = self.names()?;
        if names.iter().any(|(_, n)| n.to_bytes() == name.as_bytes()) {
            return Ok(RawName(name.to_owned()));
        }
        let found = names.into_iter().find(|(ino, _)| u128::from(*ino) == key.id);
        Ok(found.map_or_else(|| RawName(name.to_owned()), |(_, n)| RawName(OsString::from_vec(n.into_bytes()))))
    }

    fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File> {
        let c = cname(name)?;
        let mode = if write { OFlags::RDWR } else { OFlags::RDONLY };
        let fd = rustix::fs::openat(
            &self.fd,
            &c,
            mode | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(err)?;
        Ok(File::plain(fd))
    }

    /// Symlinks get an `O_PATH` handle plus the directory and name, since
    /// Linux sets their times only through `utimensat(AT_SYMLINK_NOFOLLOW)`
    /// and allows neither a mode nor `user.*` xattrs on them.
    fn open_meta(&self, name: &OsStr, kind: Kind) -> io::Result<File> {
        let c = cname(name)?;
        let flags = match kind {
            Kind::Dir => OFlags::RDONLY | OFlags::DIRECTORY,
            Kind::Symlink => OFlags::PATH,
            _ => OFlags::RDONLY | OFlags::NONBLOCK,
        };
        let fd =
            rustix::fs::openat(&self.fd, &c, flags | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty()).map_err(err)?;
        let link = if kind == Kind::Symlink { Some((self.fd.try_clone()?, c)) } else { None };
        Ok(File { fd, link })
    }

    fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        rustix::fs::mkdirat(&self.fd, &c, Mode::from_raw_mode(0o755)).map_err(err)
    }

    fn symlink(&self, target: &str, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        let t = cstr(target.as_bytes())?;
        rustix::fs::symlinkat(&t, &self.fd, &c).map_err(err)
    }

    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        rustix::fs::unlinkat(&self.fd, &c, AtFlags::empty()).map_err(err)
    }

    /// `renameat2(RENAME_NOREPLACE)`. On a case-insensitive directory a
    /// case-only rename finds "itself" at the target: that one is done with
    /// a plain `renameat`.
    fn rename_noreplace(&self, from: &OsStr, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        let f = cname(from)?;
        let t = cname(to)?;
        match rustix::fs::renameat_with(&self.fd, &f, &to_dir.fd, &t, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => {
                let a = statx_at(&self.fd, &f)?;
                let b = statx_at(&to_dir.fd, &t)?;
                let same_dir = statx_fd(&self.fd)?.stx_ino == statx_fd(&to_dir.fd)?.stx_ino;
                if same_dir && a.stx_ino == b.stx_ino && from != to {
                    rustix::fs::renameat(&self.fd, &f, &to_dir.fd, &t).map_err(err)
                } else {
                    Err(io::Error::from_raw_os_error(libc::EEXIST))
                }
            }
            Err(e) => Err(err(e)),
        }
    }

    /// `ioctl(FICLONE)` (btrfs, xfs reflink, OpenZFS block cloning), else an
    /// in-kernel copy (`copy_file_range`), into a new file with the source's mode.
    fn clone_file(&self, src: &File, name: &OsStr) -> io::Result<()> {
        let c = cname(name)?;
        let mode = src.stat()?.mode;
        let fd = rustix::fs::openat(
            &self.fd,
            &c,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode & 0o7777),
        )
        .map_err(err)?;
        let res = match rustix::fs::ioctl_ficlone(&fd, &src.fd) {
            Ok(()) => Ok(()),
            Err(_) => copy_all(src, &File::plain(fd)),
        };
        if res.is_err() {
            let _ = rustix::fs::unlinkat(&self.fd, &c, AtFlags::empty());
        }
        res
    }
}

/// An open file (or any item, for metadata).
pub struct File {
    fd: OwnedFd,
    /// For symlinks: the directory and the link's name.
    link: Option<(OwnedFd, CString)>,
}

impl File {
    fn plain(fd: OwnedFd) -> Self {
        Self { fd, link: None }
    }
}

/// Extended attributes a client may set: not the system's own bookkeeping.
fn settable_xattr(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\0')
        && !matches!(
            name,
            "com.apple.provenance" | "com.apple.ResourceFork" | "com.apple.rootless" | "com.apple.decmpfs"
        )
        && !name.starts_with("com.apple.system.")
}

fn timespec(ns: i64) -> rustix::fs::Timespec {
    rustix::fs::Timespec { tv_sec: ns.div_euclid(1_000_000_000), tv_nsec: ns.rem_euclid(1_000_000_000) as _ }
}

fn ignorable(e: rustix::io::Errno) -> bool {
    matches!(
        e,
        rustix::io::Errno::NOTSUP | rustix::io::Errno::TOOBIG | rustix::io::Errno::NOSPC | rustix::io::Errno::PERM
    )
}

impl FileOps for File {
    fn stat(&self) -> io::Result<Entry> {
        Ok(entry_from_statx(&statx_fd(&self.fd)?, RawName::default()))
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        rustix::io::pread(&self.fd, buf, offset).map_err(err)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        rustix::io::pwrite(&self.fd, buf, offset).map_err(err)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        rustix::fs::ftruncate(&self.fd, len).map_err(err)
    }

    fn sync(&self, how: Durability) -> io::Result<()> {
        match how {
            Durability::Full => rustix::fs::fsync(&self.fd),
            Durability::Barrier => rustix::fs::fdatasync(&self.fd),
        }
        .map_err(err)
    }

    /// mode: `fchmod`; xattrs: `user.<name>` (skipped where the filesystem
    /// refuses: ENOTSUP on NFS/FUSE, E2BIG/ENOSPC past ext4's 4 KiB block);
    /// tags: the Finder plist verbatim plus `user.xdg.tags`; mtime:
    /// `futimens` last. Creation time and hidden can't be set on Linux.
    fn apply_meta(&self, m: &SetMeta<'_>) -> io::Result<()> {
        if self.link.is_none() {
            for (k, v) in m.xattrs {
                if !settable_xattr(k) {
                    continue;
                }
                let name = format!("user.{k}");
                if let Err(e) = rustix::fs::fsetxattr(&self.fd, name.as_str(), v, XattrFlags::empty()) {
                    tracing::debug!(xattr = k, error = %e, "fsetxattr");
                }
            }
            if let Some(t) = m.tags {
                let set = |name: &str, v: Option<&[u8]>| -> io::Result<()> {
                    let r = match v {
                        Some(v) => rustix::fs::fsetxattr(&self.fd, name, v, XattrFlags::empty()),
                        None => match rustix::fs::fremovexattr(&self.fd, name) {
                            Err(rustix::io::Errno::NODATA) => Ok(()),
                            r => r,
                        },
                    };
                    match r {
                        Err(e) if ignorable(e) => {
                            tracing::debug!(xattr = name, error = %e, "tags not stored");
                            Ok(())
                        }
                        r => r.map_err(err),
                    }
                };
                set(TAGS_XATTR, t)?;
                let xdg = t.and_then(tags::xdg_from_plist);
                set(XDG_TAGS_XATTR, xdg.as_deref().filter(|x| !x.is_empty()))?;
            }
            if let Some(mode) = m.mode {
                rustix::fs::fchmod(&self.fd, Mode::from_raw_mode(mode & 0o7777)).map_err(err)?;
            }
        }
        if let Some(t) = m.mtime_ns {
            let ts = rustix::fs::Timestamps {
                last_access: rustix::fs::Timespec { tv_sec: 0, tv_nsec: rustix::fs::UTIME_OMIT },
                last_modification: timespec(t),
            };
            match &self.link {
                Some((dir, name)) => {
                    rustix::fs::utimensat(dir, name.as_c_str(), &ts, AtFlags::SYMLINK_NOFOLLOW).map_err(err)?
                }
                None => rustix::fs::futimens(&self.fd, &ts).map_err(err)?,
            }
        }
        Ok(())
    }
}
