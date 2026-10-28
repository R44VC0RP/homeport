//! Helpers shared by the Unix platforms (macOS and Linux).

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use homeport_proto::Kind;

use super::{ErrKind, classify_kind};

pub fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// A single path component as a C string (`EINVAL` for `/`, `.`, `..`, NUL).
pub fn cname(name: &OsStr) -> io::Result<CString> {
    let b = name.as_bytes();
    if b.is_empty() || b == b"." || b == b".." || b.contains(&b'/') || b.len() > 255 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    CString::new(b).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

pub fn cvt(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
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

pub fn ts_ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

pub fn timespec(ns: i64) -> libc::timespec {
    libc::timespec { tv_sec: ns.div_euclid(1_000_000_000) as libc::time_t, tv_nsec: ns.rem_euclid(1_000_000_000) as _ }
}

pub fn kind_from_mode(mode: libc::mode_t) -> Kind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// errno → [`ErrKind`].
pub fn classify(e: &io::Error) -> ErrKind {
    match e.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ESTALE) => ErrKind::NotFound,
        Some(libc::ENOTDIR) => ErrKind::NotADirectory,
        Some(libc::ELOOP) => ErrKind::Symlink,
        Some(libc::EEXIST) => ErrKind::Exists,
        Some(libc::ENOTEMPTY) => ErrKind::NotEmpty,
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => ErrKind::NoSpace,
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS) => ErrKind::Denied,
        Some(libc::ENAMETOOLONG) | Some(libc::EILSEQ) | Some(libc::EINVAL) => ErrKind::BadName,
        Some(libc::EBUSY) | Some(libc::ETXTBSY) => ErrKind::Busy,
        Some(libc::EXDEV) => ErrKind::CrossDevice,
        Some(libc::ENOTSUP) | Some(libc::ENOSYS) => ErrKind::Unsupported,
        Some(_) => ErrKind::Other,
        None => classify_kind(e),
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "homeport".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    let short = name.strip_suffix(".local").unwrap_or(&name);
    if short.is_empty() { "homeport".into() } else { short.to_owned() }
}

/// Local time as `YYYY-MM-DD HHMM`.
pub fn local_stamp() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: zeroed `tm` is a valid out-parameter.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    io::Write::write_all(&mut f, bytes)
}

/// Ctrl-C or SIGTERM.
pub async fn shutdown_signal() -> io::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r,
        _ = term.recv() => Ok(()),
    }
}
