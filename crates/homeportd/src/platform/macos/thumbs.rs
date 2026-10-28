//! Thumbnails with Quick Look (`qlmanage -t`).

use std::path::Path;
use std::time::Duration;

use crate::platform::ThumbnailerOps;

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Thumbnailer;

impl ThumbnailerOps for Thumbnailer {
    const AVAILABLE: bool = true;

    /// Images, video, PDF and iWork: Quick Look makes a useful picture. Other
    /// files keep their type icon on the client.
    fn wants(name: &str) -> bool {
        const EXT: &[&str] = &[
            "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "tif", "tiff", "bmp", "svg", "psd", "raw",
            "dng", "cr2", "nef", "arw", "mp4", "mov", "m4v", "mkv", "webm", "avi", "pdf", "key", "pages", "numbers",
            "usdz", "glb",
        ];
        name.rsplit_once('.').is_some_and(|(_, ext)| EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
    }

    async fn generate(path: &Path, px: u32, scratch: &Path) -> Option<Vec<u8>> {
        tokio::fs::create_dir_all(scratch).await.ok()?;
        let child = tokio::process::Command::new("/usr/bin/qlmanage")
            .arg("-t")
            .arg("-s")
            .arg(px.to_string())
            .arg("-o")
            .arg(scratch)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        let ok = matches!(tokio::time::timeout(TIMEOUT, child).await, Ok(Ok(s)) if s.success());
        let name = path.file_name()?.to_string_lossy().into_owned();
        if ok { tokio::fs::read(scratch.join(format!("{name}.png"))).await.ok() } else { None }
    }
}
