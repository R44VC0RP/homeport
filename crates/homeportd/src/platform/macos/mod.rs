//! macOS: getattrlistbulk scans, APFS file ids resolved with `fsgetpath`,
//! FSEvents with a persistent event id, `~/.Trash`, Quick Look thumbnails
//! and a LaunchAgent.

mod fs;
mod fsevents;
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

use super::{SysOps, unix};

const TAILSCALE_APP: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

pub struct Sys;

impl SysOps for Sys {
    fn home() -> Option<PathBuf> {
        unix::home()
    }

    fn data_dir(home: &Path) -> PathBuf {
        home.join("Library/Application Support/Homeport/server")
    }

    fn config_dir(home: &Path) -> PathBuf {
        home.join("Library/Application Support/Homeport")
    }

    fn cache_dir(home: &Path) -> PathBuf {
        home.join("Library/Caches/Homeport")
    }

    fn default_shares(home: &Path) -> Vec<(String, PathBuf)> {
        vec![("dev".to_owned(), home.join("dev")), ("Downloads".to_owned(), home.join("Downloads"))]
    }

    fn host_name() -> String {
        unix::host_name()
    }

    fn tailscale_candidates() -> Vec<PathBuf> {
        // The App Store / standalone app, then Homebrew's CLI.
        vec![
            PathBuf::from(TAILSCALE_APP),
            PathBuf::from("/opt/homebrew/bin/tailscale"),
            PathBuf::from("/usr/local/bin/tailscale"),
        ]
    }

    fn tailscale_command(bin: &Path) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(bin);
        // Without a terminal (e.g. under launchd) the Mac app's binary starts
        // its GUI instead of acting as the CLI unless told otherwise.
        c.env("TAILSCALE_BE_CLI", "1");
        c
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
