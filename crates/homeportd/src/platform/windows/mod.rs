//! Windows 10+ (NTFS, ReFS): `FileIdExtdDirectoryInfo` listings with 128-bit
//! file ids, handle-relative `NtCreateFile` opens, `OpenFileById` +
//! `GetFinalPathNameByHandleW` to resolve keys, `ReadDirectoryChangesExW`,
//! the Recycle Bin through `IFileOperation`, Explorer thumbnails, and a
//! scheduled task (S4U) as the service.
//!
//! Shared code handles, for every platform:
//! - item ids: `IdScheme::Assigned` makes the index assign ids and keep
//!   `FileKey → id` in its `ids` table, with the continuity rule for
//!   replace-by-rename saves (Office, JetBrains) and `replace_content`;
//! - names: `RawName` stores UTF-16; display names map the SFM private-use
//!   characters back to `" * : < > ? \ |`, escape unpaired surrogates, and
//!   `RawName::from_display` maps the other way, NFC-normalizes and refuses
//!   reserved device names (`names.rs`, `NamePolicy::Windows`).
//!
//! Gaps: no persistent change cursor (the USN journal needs elevation on
//! Windows 10), so a restart rescans indexed directories; no Finder tags or
//! xattrs (`FsCaps` says so); symlinks only with Developer Mode.

mod fs;
mod service;
mod thumbs;
mod trash;
mod watch;

use std::io;
use std::path::{Path, PathBuf};

pub use fs::{Dir, File, Fs, Volume};
pub use service::Service;
pub use thumbs::Thumbnailer;
pub use trash::Trash;
pub use watch::Watcher;

use super::SysOps;

/// Process plumbing.
pub struct Sys;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

impl SysOps for Sys {
    fn home() -> Option<PathBuf> {
        env_dir("USERPROFILE")
    }

    /// `%LOCALAPPDATA%\Homeport\server`.
    fn data_dir(home: &Path) -> PathBuf {
        env_dir("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData\\Local")).join("Homeport\\server")
    }

    /// `%APPDATA%\Homeport`.
    fn config_dir(home: &Path) -> PathBuf {
        env_dir("APPDATA").unwrap_or_else(|| home.join("AppData\\Roaming")).join("Homeport")
    }

    /// `%LOCALAPPDATA%\Homeport\cache`.
    fn cache_dir(home: &Path) -> PathBuf {
        env_dir("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData\\Local")).join("Homeport\\cache")
    }

    /// None: shares come from `homeportd share add`.
    fn default_shares(_home: &Path) -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    fn host_name() -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "homeport".into())
    }

    fn tailscale_candidates() -> Vec<PathBuf> {
        let pf = env_dir("ProgramFiles").unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        vec![pf.join("Tailscale\\tailscale.exe")]
    }

    /// `CREATE_NO_WINDOW` (0x08000000) keeps a console from flashing when run
    /// as a scheduled task.
    fn tailscale_command(bin: &Path) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(bin);
        c.creation_flags(0x0800_0000);
        c
    }

    /// Ctrl-C, or the console/service shutdown (`tokio::signal::windows::ctrl_shutdown`).
    async fn shutdown_signal() -> io::Result<()> {
        let mut shutdown = tokio::signal::windows::ctrl_shutdown()?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = shutdown.recv() => Ok(()),
        }
    }

    /// Local wall-clock time (`GetLocalTime`).
    fn local_stamp() -> String {
        // SAFETY: zeroed SYSTEMTIME is a valid out-parameter.
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        // SAFETY: `t` is writable.
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
        format!("{:04}-{:02}-{:02} {:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
    }

    /// `%LOCALAPPDATA%` is only readable by the user; ACLs inherit.
    fn create_private_dir(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
}
