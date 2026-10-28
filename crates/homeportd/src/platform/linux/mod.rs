//! Linux (ext4, btrfs, xfs, zfs): `getdents64` + `statx` scans, index-assigned
//! item ids (`IdScheme::Assigned`; `Volume::path_of` is `None`, the index
//! walks its rows), `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` inside
//! shares, inotify on indexed directories, the freedesktop Trash, thumbnails
//! with the `image` crate and `ffmpeg`, and a systemd user unit.

mod fs;
mod tags;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

pub use fs::{Dir, File, Fs, Volume};

use tokio::sync::mpsc;

use super::{ServiceOps, SysOps, ThumbnailerOps, TrashOps, WatchBatch, WatcherOps, unix, unsupported};

/// Process plumbing.
pub struct Sys;

fn xdg(var: &str, home: &Path, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(default))
}

impl SysOps for Sys {
    fn home() -> Option<PathBuf> {
        unix::home()
    }

    /// `$XDG_STATE_HOME/homeport/server` (`~/.local/state/homeport/server`).
    fn data_dir(home: &Path) -> PathBuf {
        xdg("XDG_STATE_HOME", home, ".local/state").join("homeport/server")
    }

    /// `$XDG_CONFIG_HOME/homeport` (`~/.config/homeport`).
    fn config_dir(home: &Path) -> PathBuf {
        xdg("XDG_CONFIG_HOME", home, ".config").join("homeport")
    }

    /// `$XDG_CACHE_HOME/homeport` (`~/.cache/homeport`).
    fn cache_dir(home: &Path) -> PathBuf {
        xdg("XDG_CACHE_HOME", home, ".cache").join("homeport")
    }

    /// None: shares come from `homeportd share add`.
    fn default_shares(_home: &Path) -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    fn host_name() -> String {
        unix::host_name()
    }

    fn tailscale_candidates() -> Vec<PathBuf> {
        vec![PathBuf::from("/usr/bin/tailscale"), PathBuf::from("/usr/local/bin/tailscale")]
    }

    fn tailscale_command(bin: &Path) -> tokio::process::Command {
        tokio::process::Command::new(bin)
    }

    async fn shutdown_signal() -> io::Result<()> {
        unix::shutdown_signal().await
    }

    fn local_stamp() -> String {
        unix::local_stamp()
    }

    fn create_private_dir(path: &Path) -> io::Result<()> {
        unix::create_private_dir(path)
    }

    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        unix::write_private(path, bytes)
    }
}

/// inotify on indexed directories only.
pub struct Watcher;

impl WatcherOps for Watcher {
    /// One inotify fd and a reader thread. Ignore `cursor` (there is none):
    /// send `Change::All` once so every indexed directory is rescanned and
    /// diffed against the index. Check `/proc/sys/fs/inotify/max_user_watches`
    /// and log headroom.
    fn start(_roots: &[PathBuf], _cursor: Option<&str>, _tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        Err(unsupported("watching"))
    }

    /// `inotify_add_watch(path, IN_CREATE|IN_DELETE|IN_MOVED_FROM|IN_MOVED_TO|
    /// IN_CLOSE_WRITE|IN_ATTRIB|IN_MODIFY|IN_DELETE_SELF|IN_MOVE_SELF|
    /// IN_ONLYDIR|IN_EXCL_UNLINK)`, then send `Change::Path(path)` (the
    /// create-then-watch race). Events map to `Change::Path(dir/name)`;
    /// coalesce IN_MODIFY per ~100 ms; `IN_Q_OVERFLOW` → `Change::All`;
    /// `IN_IGNORED`/`IN_DELETE_SELF`/`IN_UNMOUNT` → `Change::Path(dir)`.
    fn watch_dir(&self, _path: &Path) {}

    /// `inotify_rm_watch`.
    fn unwatch_dir(&self, _path: &Path) {}
}

/// freedesktop.org Trash.
pub struct Trash;

impl TrashOps for Trash {
    /// Home trash `$XDG_DATA_HOME/Trash` (`~/.local/share/Trash`) with
    /// `files/` and `info/`.
    fn open(_home: &Path, _data_dir: &Path) -> io::Result<Self> {
        Err(unsupported("trash"))
    }

    /// Same filesystem as the home trash → home trash; else
    /// `$topdir/.Trash/$uid` (sticky, not a symlink) or `$topdir/.Trash-$uid`.
    /// Write `info/<name>.trashinfo` with `O_EXCL` (Path=percent-encoded
    /// absolute path, DeletionDate) first, then `renameat2(NOREPLACE)` into
    /// `files/<name>`; on collision add ` 2`, ` 3`... Return the new path.
    fn trash(&self, _dir: &Dir, _name: &OsStr) -> io::Result<PathBuf> {
        Err(unsupported("trash"))
    }

    /// Under any trash `files/` dir this instance uses.
    fn contains(&self, _path: &Path) -> bool {
        false
    }

    /// `renameat2(NOREPLACE)` back, then remove the `.trashinfo`.
    fn put_back(&self, _from: &Path, _to_dir: &Dir, _to: &OsStr) -> io::Result<()> {
        Err(unsupported("put back"))
    }
}

/// Thumbnails.
pub struct Thumbnailer;

impl ThumbnailerOps for Thumbnailer {
    /// Set to true once `generate` works.
    const AVAILABLE: bool = false;

    fn wants(_name: &str) -> bool {
        false
    }

    /// First the freedesktop cache (`~/.cache/thumbnails/{large,x-large}/
    /// md5(file URI).png`, valid when `Thumb::MTime` matches). Then: `image` +
    /// `fast_image_resize` for raster formats, `ffmpeg -ss 1 -frames:v 1` for
    /// video, `pdftoppm` for PDF, or the system thumbnailers in
    /// `/usr/share/thumbnailers/*.thumbnailer` (`Exec=` with `%i %o %s`).
    async fn generate(_path: &Path, _px: u32, _scratch: &Path) -> Option<Vec<u8>> {
        None
    }
}

/// systemd user unit.
pub struct Service;

impl ServiceOps for Service {
    /// Write `~/.config/systemd/user/homeportd.service` (`ExecStart=exe args`,
    /// `Restart=always`, `RestartSec=5`, `Type=simple` or `notify` with
    /// `sd-notify`), `systemctl --user daemon-reload`, `enable --now`, and
    /// tell the user to run `loginctl enable-linger $USER` so it runs without
    /// a login. With `print`, return the unit text only. Also suggest the
    /// `net.core.rmem_max/wmem_max=7500000` sysctl for QUIC.
    fn install(_exe: &Path, _args: &[OsString], _print: bool) -> io::Result<String> {
        Err(unsupported("service install"))
    }

    /// `systemctl --user disable --now homeportd`, remove the unit, daemon-reload.
    fn uninstall() -> io::Result<String> {
        Err(unsupported("service uninstall"))
    }

    /// `systemctl --user show homeportd -p ActiveState,SubState,MainPID`.
    fn status() -> io::Result<String> {
        Err(unsupported("service status"))
    }
}
