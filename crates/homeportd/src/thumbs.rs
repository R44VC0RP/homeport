//! Thumbnails made on basebook with Quick Look (`qlmanage -t`), so clients
//! draw image and video icons without downloading the files. Cached on disk
//! by item, content revision and size; failures are cached too.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::index::{Index, IndexError};

/// Concurrent `qlmanage` processes.
const JOBS: usize = 6;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Largest edge clients may ask for.
const MAX_PIXELS: u32 = 512;

pub struct Thumbs {
    dir: PathBuf,
    jobs: Semaphore,
}

static THUMBS: OnceLock<Thumbs> = OnceLock::new();

/// Sets the cache directory; call once at startup.
pub fn init(dir: PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let _ = THUMBS.set(Thumbs { dir, jobs: Semaphore::new(JOBS) });
    Ok(())
}

/// Whether Quick Look makes a useful picture from this kind of file (images,
/// video, PDF). Other files keep their type icon on the client.
pub fn wants(name: &str) -> bool {
    const EXT: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "tif", "tiff", "bmp", "svg", "psd", "raw", "dng",
        "cr2", "nef", "arw", "mp4", "mov", "m4v", "mkv", "webm", "avi", "pdf", "key", "pages", "numbers", "usdz", "glb",
    ];
    name.rsplit_once('.').is_some_and(|(_, ext)| EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// A PNG thumbnail no larger than `max_pixels` on its long edge, or `None`
/// when Quick Look has none for the file.
pub async fn get(index: &std::sync::Arc<Index>, id: &str, max_pixels: u32) -> Result<Option<Vec<u8>>, IndexError> {
    let Some(t) = THUMBS.get() else { return Ok(None) };
    let idx = index.clone();
    let idc = id.to_owned();
    let (path, content_rev) = tokio::task::spawn_blocking(move || idx.file_path(&idc))
        .await??;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !wants(&name) {
        return Ok(None);
    }
    // Sizes in steps of 64 so nearby requests share one cache entry.
    let px = max_pixels.clamp(32, MAX_PIXELS).div_ceil(64) * 64;
    let key = format!("{id}-{content_rev}-{px}");
    let png = t.dir.join(format!("{key}.png"));
    let none = t.dir.join(format!("{key}.none"));
    if let Ok(b) = tokio::fs::read(&png).await {
        return Ok(Some(b));
    }
    if tokio::fs::try_exists(&none).await.unwrap_or(false) {
        return Ok(None);
    }
    // Detached, so a client that gives up (deadline, cancelled scroll)
    // still leaves the thumbnail cached for its next request.
    let task = tokio::spawn(async move {
        let _permit = t.jobs.acquire().await.ok()?;
        // Another request may have made it while this one waited.
        if let Ok(b) = tokio::fs::read(&png).await {
            return Some(b);
        }
        match generate(&path, px, &t.dir, &key).await {
            Some(b) => {
                let tmp = t.dir.join(format!("{key}.tmp"));
                if tokio::fs::write(&tmp, &b).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &png).await;
                }
                Some(b)
            }
            None => {
                let _ = tokio::fs::write(&none, b"").await;
                None
            }
        }
    });
    Ok(task.await?)
}

async fn generate(path: &Path, px: u32, dir: &Path, key: &str) -> Option<Vec<u8>> {
    let out = dir.join(format!("{key}.d"));
    tokio::fs::create_dir_all(&out).await.ok()?;
    let started = std::time::Instant::now();
    let child = tokio::process::Command::new("/usr/bin/qlmanage")
        .arg("-t")
        .arg("-s")
        .arg(px.to_string())
        .arg("-o")
        .arg(&out)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    let ok = matches!(tokio::time::timeout(TIMEOUT, child).await, Ok(Ok(s)) if s.success());
    let name = path.file_name()?.to_string_lossy().into_owned();
    let result = if ok { tokio::fs::read(out.join(format!("{name}.png"))).await.ok() } else { None };
    let _ = tokio::fs::remove_dir_all(&out).await;
    tracing::debug!(key, ok = result.is_some(), ms = started.elapsed().as_millis() as u64, "thumbnail");
    result
}
