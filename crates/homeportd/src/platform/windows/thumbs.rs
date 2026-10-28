//! Explorer's thumbnails: `IShellItemImageFactory::GetImage` (the same
//! providers and thumbnail cache Explorer uses, so HEIC/RAW work where their
//! codecs are installed), encoded to PNG with WIC. Each request runs on its
//! own COM single-threaded apartment thread. Cloud placeholders are never
//! asked for a picture (that would download them).

use std::path::Path;
use std::time::Duration;

use homeport_proto::{Kind, item_flags};
use windows::Win32::Foundation::{HGLOBAL, SIZE};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ, HPALETTE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA, IWICBitmapSource,
    IWICImagingFactory, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant, WICBitmapUseAlpha,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
};
use windows::core::{Interface, PCWSTR};

use super::fs::Fs;
use crate::platform::{FsOps, ThumbnailerOps};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Thumbnailer;

/// A path the shell parses (without `\\?\`), NUL-terminated.
fn shell_path(path: &Path) -> Vec<u16> {
    let s = path.as_os_str().to_string_lossy();
    let plain = match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned(),
    };
    plain.encode_utf16().chain(Some(0)).collect()
}

/// The thumbnail as PNG, at most `px` on the long edge.
fn thumbnail(path: &[u16], px: u32) -> windows::core::Result<Vec<u8>> {
    // SAFETY: COM calls with valid arguments on an STA thread.
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None)?;
        let side = i32::try_from(px).unwrap_or(256);
        let hbmp = factory.GetImage(SIZE { cx: side, cy: side }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK)?;
        let r = (|| {
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let bmp = wic.CreateBitmapFromHBITMAP(hbmp, HPALETTE::default(), WICBitmapUseAlpha)?;
            let (mut w, mut h) = (0u32, 0u32);
            bmp.GetSize(&mut w, &mut h)?;
            let mut source: IWICBitmapSource = bmp.cast()?;
            if w.max(h) > px && w > 0 && h > 0 {
                let scale = f64::from(px) / f64::from(w.max(h));
                let (nw, nh) =
                    (((f64::from(w) * scale).round() as u32).max(1), ((f64::from(h) * scale).round() as u32).max(1));
                let scaler = wic.CreateBitmapScaler()?;
                scaler.Initialize(&source, nw, nh, WICBitmapInterpolationModeFant)?;
                source = scaler.cast()?;
                (w, h) = (nw, nh);
            }
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            let mut props = None;
            encoder.CreateNewFrame(&mut frame, &mut props)?;
            let frame = frame.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
            frame.Initialize(props.as_ref())?;
            frame.SetSize(w, h)?;
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut fmt)?;
            frame.WriteSource(&source, std::ptr::null())?;
            frame.Commit()?;
            encoder.Commit()?;
            let hg = GetHGlobalFromStream(&stream)?;
            let len = GlobalSize(hg);
            let p = GlobalLock(hg);
            if p.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let bytes = std::slice::from_raw_parts(p.cast::<u8>(), len).to_vec();
            let _ = GlobalUnlock(hg);
            Ok(bytes)
        })();
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        r
    }
}

impl ThumbnailerOps for Thumbnailer {
    const AVAILABLE: bool = true;

    /// Images, video and PDF (where a handler is installed): other files keep
    /// their type icon on the client.
    fn wants(name: &str) -> bool {
        const EXT: &[&str] = &[
            "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "tif", "tiff", "bmp", "ico", "svg", "psd",
            "raw", "dng", "cr2", "nef", "arw", "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "pdf",
        ];
        name.rsplit_once('.').is_some_and(|(_, ext)| EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
    }

    async fn generate(path: &Path, px: u32, _scratch: &Path) -> Option<Vec<u8>> {
        let e = Fs::lstat(path).ok()?;
        if e.kind != Kind::File || e.flags & item_flags::CLOUD_ONLY != 0 {
            return None;
        }
        let wide = shell_path(path);
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("homeport-thumb".into())
            .spawn(move || {
                // SAFETY: COM is initialized and uninitialized on this thread only.
                let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
                if init.is_err() {
                    return;
                }
                let r = thumbnail(&wide, px);
                // SAFETY: paired with CoInitializeEx.
                unsafe { CoUninitialize() };
                match r {
                    Ok(png) => {
                        let _ = tx.send(png);
                    }
                    Err(e) => tracing::debug!(error = %e, "no thumbnail"),
                }
            })
            .ok()?;
        tokio::time::timeout(TIMEOUT, rx).await.ok()?.ok()
    }
}
