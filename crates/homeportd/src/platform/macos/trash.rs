//! The user's Trash: `~/.Trash` for the home volume, `<volume>/.Trashes/<uid>`
//! for others, with Finder-style names for duplicates (`a.txt`, `a 2.txt`).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::fs::Dir;
use crate::platform::unix::{classify, cpath};
use crate::platform::{ErrKind, TrashOps};

pub struct Trash {
    /// `~/.Trash` (canonical).
    home: PathBuf,
}

/// Finder-style names for the Trash: `a.txt`, `a 2.txt`, `a 3.txt`, ...
fn trash_name(name: &OsStr, n: u32) -> OsString {
    if n <= 1 {
        return name.to_owned();
    }
    let b = name.as_bytes();
    match b.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 => {
            let mut v = b[..i].to_vec();
            v.extend_from_slice(format!(" {n}").as_bytes());
            v.extend_from_slice(&b[i..]);
            OsString::from_vec(v)
        }
        _ => {
            let mut v = b.to_vec();
            v.extend_from_slice(format!(" {n}").as_bytes());
            OsString::from_vec(v)
        }
    }
}

/// Mount point of the volume holding `path`.
fn mount_point(path: &Path) -> io::Result<PathBuf> {
    let c = cpath(path)?;
    // SAFETY: zeroed `statfs` is a valid out-parameter; `c` is NUL-terminated.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    crate::platform::unix::cvt(unsafe { libc::statfs(c.as_ptr(), &mut sfs) })?;
    // SAFETY: `f_mntonname` is NUL-terminated.
    let name = unsafe { std::ffi::CStr::from_ptr(sfs.f_mntonname.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

impl Trash {
    /// Moves into `dir_path` under the first free Finder-style name.
    fn move_into(&self, dir: &Dir, name: &OsStr, trash: &Path) -> io::Result<PathBuf> {
        for n in 1..1000 {
            let to = trash.join(trash_name(name, n));
            match dir.rename_to_path_excl(name, &to) {
                Ok(()) => return Ok(to),
                Err(e) if classify(&e) == ErrKind::Exists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no free name in the Trash"))
    }

    /// `<volume>/.Trashes/<uid>`, created like Finder does.
    fn volume_trash(&self, item_dir: &Path) -> io::Result<PathBuf> {
        let mp = mount_point(item_dir)?;
        // SAFETY: no preconditions.
        let uid = unsafe { libc::getuid() };
        let trashes = mp.join(".Trashes");
        if !trashes.exists() {
            std::fs::create_dir(&trashes)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&trashes, std::fs::Permissions::from_mode(0o1333))?;
        }
        let mine = trashes.join(uid.to_string());
        if !mine.exists() {
            std::fs::create_dir(&mine)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(mine)
    }
}

impl TrashOps for Trash {
    fn open(home: &Path, _data_dir: &Path) -> io::Result<Self> {
        let trash = home.join(".Trash");
        std::fs::create_dir_all(&trash)?;
        Ok(Self { home: std::fs::canonicalize(&trash)? })
    }

    fn trash(&self, dir: &Dir, name: &OsStr) -> io::Result<PathBuf> {
        match self.move_into(dir, name, &self.home) {
            Err(e) if classify(&e) == ErrKind::CrossDevice => {
                let dir_path = dir.current_path()?;
                let trash = self.volume_trash(&dir_path)?;
                self.move_into(dir, name, &trash)
            }
            r => r,
        }
    }

    fn contains(&self, path: &Path) -> bool {
        if path.starts_with(&self.home) {
            return true;
        }
        let mut comps = path.components().rev().skip(1);
        // `<volume>/.Trashes/<uid>/<name>`
        comps.next().is_some() && comps.next().is_some_and(|c| c.as_os_str() == ".Trashes")
    }

    fn put_back(&self, from: &Path, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        to_dir.rename_from_path_excl(from, to)
    }
}
