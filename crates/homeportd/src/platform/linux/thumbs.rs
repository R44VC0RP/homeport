//! Thumbnails without Quick Look: raster images with the `image` crate (in
//! process), video with `ffmpeg` and PDF with `pdftoppm` when installed.
//! Files of other types (or video/PDF without the tool) get no thumbnail and
//! keep their type icon on the client.

use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use rustix::fs::OFlags;

use super::fs::open_confined;
use crate::platform::ThumbnailerOps;

const TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico"];
const VIDEO_EXT: &[&str] = &["mp4", "mov", "m4v", "mkv", "webm", "avi"];
/// Images with more pixels than this aren't decoded (memory).
const MAX_DECODE_BYTES: u64 = 512 << 20;

fn ext_in(name: &str, list: &[&str]) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| list.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// A tool from the usual places or `$PATH`.
fn find_tool(name: &str) -> Option<PathBuf> {
    let dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    ["/usr/bin", "/usr/local/bin", "/bin"]
        .iter()
        .map(PathBuf::from)
        .chain(dirs)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn ffmpeg() -> Option<&'static Path> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| find_tool("ffmpeg")).as_deref()
}

fn pdftoppm() -> Option<&'static Path> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| find_tool("pdftoppm")).as_deref()
}

fn image_thumb(path: &Path, px: u32) -> Option<Vec<u8>> {
    let file = std::fs::File::from(open_confined(path, OFlags::RDONLY | OFlags::NONBLOCK).ok()?);
    let mut reader = image::ImageReader::new(BufReader::new(file)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = if img.width() > px || img.height() > px { img.thumbnail(px, px) } else { img };
    let mut out = Cursor::new(Vec::new());
    thumb.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Runs a helper with a timeout (killed on drop) and returns whether it succeeded.
async fn run(cmd: &mut tokio::process::Command) -> bool {
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(tokio::time::timeout(TIMEOUT, child).await, Ok(Ok(s)) if s.success())
}

async fn video_thumb(bin: &Path, path: &Path, px: u32, scratch: &Path) -> Option<Vec<u8>> {
    let out = scratch.join("frame.png");
    let scale = format!("scale={px}:{px}:force_original_aspect_ratio=decrease");
    // One second in skips black first frames; short clips fall back to the start.
    for at in ["1", "0"] {
        let ok = run(tokio::process::Command::new(bin)
            .args(["-nostdin", "-loglevel", "error", "-y", "-ss", at, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", &scale])
            .arg(&out))
        .await;
        if ok && let Ok(b) = tokio::fs::read(&out).await {
            return Some(b);
        }
    }
    None
}

async fn pdf_thumb(bin: &Path, path: &Path, px: u32, scratch: &Path) -> Option<Vec<u8>> {
    let prefix = scratch.join("page");
    let ok = run(tokio::process::Command::new(bin)
        .args(["-png", "-singlefile", "-f", "1", "-l", "1", "-scale-to"])
        .arg(px.to_string())
        .arg(path)
        .arg(&prefix))
    .await;
    if ok { tokio::fs::read(scratch.join("page.png")).await.ok() } else { None }
}

pub struct Thumbnailer;

impl ThumbnailerOps for Thumbnailer {
    const AVAILABLE: bool = true;

    fn wants(name: &str) -> bool {
        ext_in(name, IMAGE_EXT)
            || (ffmpeg().is_some() && ext_in(name, VIDEO_EXT))
            || (pdftoppm().is_some() && ext_in(name, &["pdf"]))
    }

    async fn generate(path: &Path, px: u32, scratch: &Path) -> Option<Vec<u8>> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        if ext_in(&name, IMAGE_EXT) {
            let p = path.to_owned();
            let job = tokio::task::spawn_blocking(move || image_thumb(&p, px));
            return tokio::time::timeout(TIMEOUT, job).await.ok()?.ok()?;
        }
        tokio::fs::create_dir_all(scratch).await.ok()?;
        if ext_in(&name, VIDEO_EXT) {
            return video_thumb(ffmpeg()?, path, px, scratch).await;
        }
        if ext_in(&name, &["pdf"]) {
            return pdf_thumb(pdftoppm()?, path, px, scratch).await;
        }
        None
    }
}
