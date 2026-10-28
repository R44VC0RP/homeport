//! The freedesktop.org Trash (<https://specifications.freedesktop.org/trash-spec/latest/>):
//! the home trash `$XDG_DATA_HOME/Trash` for items on the home filesystem,
//! `$topdir/.Trash/$uid` or `$topdir/.Trash-$uid` for other filesystems.
//! The `.trashinfo` is written first (`O_EXCL` reserves the name), then the
//! item is renamed in with `RENAME_NOREPLACE`. Nothing is ever deleted.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};

use super::fs::{Dir, err, mount_point_of};
use crate::platform::unix::cname;
use crate::platform::{ErrKind, TrashOps};

/// Longest name in `files/` so that `<name>.trashinfo` fits in 255 bytes.
const MAX_NAME: usize = 255 - ".trashinfo".len();

pub struct Trash {
    /// The home trash (canonical), with `files/` and `info/`.
    home: PathBuf,
    home_dev: u64,
    uid: u32,
}

/// `a.txt`, `a 2.txt`, `a 3.txt`, ... shortened to fit `MAX_NAME`.
fn trash_name(name: &OsStr, n: u32) -> OsString {
    let b = name.as_bytes();
    let suffix = if n <= 1 { String::new() } else { format!(" {n}") };
    let (stem, ext) = match b.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 && b.len() - i <= 32 => (&b[..i], &b[i..]),
        _ => (b, &b[..0]),
    };
    let room = MAX_NAME.saturating_sub(suffix.len() + ext.len());
    let mut stem = &stem[..stem.len().min(room)];
    // Don't cut a UTF-8 sequence in half when the name is UTF-8.
    while !stem.is_empty() && std::str::from_utf8(b).is_ok() && std::str::from_utf8(stem).is_err() {
        stem = &stem[..stem.len() - 1];
    }
    let mut v = stem.to_vec();
    v.extend_from_slice(suffix.as_bytes());
    v.extend_from_slice(ext);
    OsString::from_vec(v)
}

/// Percent-encodes a path for `Path=` (RFC 2396 escaping, `/` kept).
fn encode_path(p: &[u8]) -> String {
    let mut s = String::with_capacity(p.len());
    for &c in p {
        if c.is_ascii_alphanumeric() || b"-_.~/".contains(&c) {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}

/// Local time as `YYYY-MM-DDThh:mm:ss`.
fn deletion_date() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: a zeroed `tm` is a valid out-parameter.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    rustix::fs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .map_err(err)
}

/// Creates `files/` and `info/` under a trash directory.
fn ensure_layout(trash: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(trash.join("files"))?;
    b.create(trash.join("info"))
}

/// Whether `p` is a real directory owned by `uid` (not a symlink).
fn own_dir(p: &Path, uid: u32) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.uid() == uid)
}

impl Trash {
    /// The trash for a filesystem whose root is `top`, and the base that
    /// `Path=` is relative to.
    fn top_trash(&self, top: &Path) -> io::Result<PathBuf> {
        // `$topdir/.Trash/$uid`, when an admin made `.Trash` (sticky, not a symlink).
        let shared = top.join(".Trash");
        if let Ok(m) = std::fs::symlink_metadata(&shared)
            && m.is_dir()
            && m.mode() & 0o1000 != 0
        {
            let mine = shared.join(self.uid.to_string());
            if !mine.exists() {
                let _ = std::fs::DirBuilder::new().mode(0o700).create(&mine);
            }
            if own_dir(&mine, self.uid) && ensure_layout(&mine).is_ok() {
                return Ok(mine);
            }
        }
        let mine = top.join(format!(".Trash-{}", self.uid));
        if !mine.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&mine)?;
        }
        if !own_dir(&mine, self.uid) {
            return Err(io::Error::other(format!("{} is not this user's trash directory", mine.display())));
        }
        ensure_layout(&mine)?;
        Ok(mine)
    }

    /// Moves `name` from `dir` into `trash`; `info_path` goes in `Path=`.
    fn move_into(&self, dir: &Dir, name: &OsStr, trash: &Path, info_path: &[u8]) -> io::Result<PathBuf> {
        let files = open_dir(&trash.join("files"))?;
        let info = open_dir(&trash.join("info"))?;
        let from = cname(name)?;
        let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode_path(info_path), deletion_date());
        for n in 1..10_000 {
            let cand = trash_name(name, n);
            let mut info_name = cand.clone();
            info_name.push(".trashinfo");
            let info_c = cname(&info_name)?;
            let fd = match rustix::fs::openat(
                &info,
                &info_c,
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::EXIST) => continue,
                Err(e) => return Err(err(e)),
            };
            let mut f = std::fs::File::from(fd);
            let wrote = f.write_all(body.as_bytes()).and_then(|()| f.sync_all());
            let to = cname(&cand)?;
            let moved = wrote.and_then(|()| {
                rustix::fs::renameat_with(&dir.fd, &from, &files, &to, RenameFlags::NOREPLACE).map_err(err)
            });
            match moved {
                Ok(()) => return Ok(trash.join("files").join(&cand)),
                Err(e) => {
                    let _ = rustix::fs::unlinkat(&info, &info_c, AtFlags::empty());
                    if e.raw_os_error() != Some(libc::EEXIST) {
                        return Err(e);
                    }
                }
            }
        }
        Err(io::Error::other("no free name in the trash"))
    }

    fn trash_on_top(&self, dir: &Dir, name: &OsStr, full: &Path) -> io::Result<PathBuf> {
        let top = mount_point_of(&dir.statx()?).ok_or_else(|| io::Error::other("no mount point for the item"))?;
        let trash = self.top_trash(&top)?;
        let rel = full.strip_prefix(&top).unwrap_or(full);
        self.move_into(dir, name, &trash, rel.as_os_str().as_bytes())
    }
}

impl TrashOps for Trash {
    fn open(home: &Path, _data_dir: &Path) -> io::Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        let trash = base.join("Trash");
        ensure_layout(&trash)?;
        let home = std::fs::canonicalize(&trash)?;
        let home_dev = std::fs::metadata(&home)?.dev();
        // SAFETY: no preconditions.
        let uid = unsafe { libc::getuid() };
        Ok(Self { home, home_dev, uid })
    }

    fn trash(&self, dir: &Dir, name: &OsStr) -> io::Result<PathBuf> {
        let st = dir.statx()?;
        let full = dir.current_path()?.join(name);
        if libc::makedev(st.stx_dev_major, st.stx_dev_minor) == self.home_dev {
            match self.move_into(dir, name, &self.home, full.as_os_str().as_bytes()) {
                // A btrfs subvolume shares the device number's filesystem but not renames.
                Err(e) if crate::platform::unix::classify(&e) == ErrKind::CrossDevice => {}
                r => return r,
            }
        }
        self.trash_on_top(dir, name, &full)
    }

    fn contains(&self, path: &Path) -> bool {
        if path.starts_with(self.home.join("files")) {
            return true;
        }
        let uid = self.uid.to_string();
        let mine = format!(".Trash-{uid}");
        let comps: Vec<&OsStr> = path.components().map(|c| c.as_os_str()).collect();
        comps.windows(3).any(|w| {
            (w[1] == OsStr::new(&mine) && w[2] == "files")
                || (w[0] == ".Trash" && w[1] == OsStr::new(&uid) && w[2] == "files")
        })
    }

    fn put_back(&self, from: &Path, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        let t = cname(to)?;
        rustix::fs::renameat_with(rustix::fs::CWD, from, &to_dir.fd, &t, RenameFlags::NOREPLACE).map_err(err)?;
        if let (Some(files), Some(n)) = (from.parent(), from.file_name())
            && files.file_name() == Some(OsStr::new("files"))
            && let Some(trash) = files.parent()
        {
            let mut info = n.to_owned();
            info.push(".trashinfo");
            let _ = std::fs::remove_file(trash.join("info").join(info));
        }
        Ok(())
    }
}
