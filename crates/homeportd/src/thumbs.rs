//! Thumbnails made on the server (macOS: Quick Look), so clients draw image
//! and video icons without downloading the files. Cached on disk by item,
//! content revision and size; failures are cached too.

use std::path::PathBuf;
use std::sync::OnceLock;

use tokio::sync::Semaphore;

use crate::index::{Index, IndexError};
use crate::platform::{Thumbnailer, ThumbnailerOps};

/// Concurrent thumbnail jobs.
const JOBS: usize = 6;
/// Largest edge clients may ask for.
const MAX_PIXELS: u32 = 512;

pub struct Thumbs {
    dir: PathBuf,
    jobs: Semaphore,
}

static THUMBS: OnceLock<Thumbs> = OnceLock::new();

/// Sets the cache directory; call once at startup.
pub fn init(dir: PathBuf) -> std::io::Result<()> {
    if !Thumbnailer::AVAILABLE {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let _ = THUMBS.set(Thumbs { dir, jobs: Semaphore::new(JOBS) });
    Ok(())
}

/// Whether the server makes thumbnails.
pub fn available() -> bool {
    THUMBS.get().is_some()
}

/// A PNG thumbnail no larger than `max_pixels` on its long edge, or `None`
/// when there is none for the file.
pub async fn get(index: &std::sync::Arc<Index>, id: &str, max_pixels: u32) -> Result<Option<Vec<u8>>, IndexError> {
    let Some(t) = THUMBS.get() else { return Ok(None) };
    let idx = index.clone();
    let idc = id.to_owned();
    let (path, content_rev) = tokio::task::spawn_blocking(move || idx.file_path(&idc)).await??;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !Thumbnailer::wants(&name) {
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
        let scratch = t.dir.join(format!("{key}.d"));
        let started = std::time::Instant::now();
        let made = Thumbnailer::generate(&path, px, &scratch).await;
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        tracing::debug!(key, ok = made.is_some(), ms = started.elapsed().as_millis() as u64, "thumbnail");
        match made {
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
