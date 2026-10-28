//! Linux (ext4, btrfs, xfs, zfs): `getdents64` + `statx` scans, index-assigned
//! item ids (`IdScheme::Assigned`; `Volume::path_of` is `None`, the index
//! walks its rows), `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` inside
//! shares, inotify on indexed directories, the freedesktop Trash, thumbnails
//! with the `image` crate and `ffmpeg`, and a systemd user unit.

mod fs;
mod tags;
mod thumbs;
mod trash;
mod watch;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

pub use fs::{Dir, File, Fs, Volume};
pub use thumbs::Thumbnailer;
pub use trash::Trash;
pub use watch::Watcher;

use super::{ServiceOps, SysOps, unix, unsupported};

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
