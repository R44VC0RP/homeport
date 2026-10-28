//! The Recycle Bin, through `IFileOperation` (what Explorer's Delete does), on
//! a COM single-threaded apartment thread per operation.
//!
//! Never a permanent delete. With no UI, Windows silently destroys items that
//! don't fit the bin, or every item when the bin is turned off. So before an
//! item goes, it is refused when the volume's bin is off (`NukeOnDelete`, the
//! `NoRecycleFiles` policy), when it is larger than the bin (`MaxCapacity`),
//! or when its path is too long for the shell; and a progress sink aborts the
//! delete if the shell decides not to recycle anyway. The item keeps its
//! FileId in the bin, which is how we find where it went.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use homeport_proto::Kind;
use windows::Win32::Foundation::E_ABORT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation,
    IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{HRESULT, PCWSTR, Ref, implement};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES};

use super::fs::{Dir, Volume, busy_retry, final_path, open_path, volume_guid};
use crate::platform::{DirOps, TrashOps};

/// Shell paths longer than this can't be recycled reliably.
const SHELL_MAX_PATH: usize = 259;

pub struct Trash;

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let k: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let v: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut data: u32 = 0;
    let mut len = 4u32;
    // SAFETY: strings are NUL-terminated; `data` is writable for `len` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(data)
}

fn refuse(msg: String) -> io::Error {
    tracing::warn!("{msg}");
    io::Error::new(io::ErrorKind::Unsupported, msg)
}

/// Size of an item, stopping once it passes `limit`. Never follows links.
fn size_upto(path: &Path, limit: u64) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_owned()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(e.path());
            } else if let Ok(m) = e.metadata() {
                total += m.len();
                if total > limit {
                    return total;
                }
            }
        }
    }
    total
}

/// Refuses items the Recycle Bin would destroy instead of keeping.
fn check_recyclable(path: &Path, kind: Kind, size: u64) -> io::Result<()> {
    const POLICY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer";
    if reg_dword(POLICY, "NoRecycleFiles") == Some(1) {
        return Err(refuse("the Recycle Bin is turned off by policy (NoRecycleFiles): not deleting".into()));
    }
    let guid = volume_guid(path)
        .and_then(|g| g.find('{').map(|i| g[i..].trim_end_matches('\\').to_owned()))
        .ok_or_else(|| refuse(format!("no Recycle Bin for {}: not deleting", path.display())))?;
    let key = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\BitBucket\\Volume\\{guid}");
    if reg_dword(&key, "NukeOnDelete") == Some(1) {
        return Err(refuse(format!(
            "the Recycle Bin is set to delete files immediately on this drive ({guid}): not deleting"
        )));
    }
    if let Some(mb) = reg_dword(&key, "MaxCapacity") {
        let limit = u64::from(mb) << 20;
        let size = if kind == Kind::Dir { size_upto(path, limit) } else { size };
        if size > limit {
            return Err(refuse(format!(
                "{} is larger than the Recycle Bin ({mb} MB) and would be deleted permanently: not deleting",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The path the shell understands: without `\\?\`.
fn shell_path(path: &Path) -> io::Result<Vec<u16>> {
    let s = path.as_os_str().to_string_lossy();
    let plain = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
    };
    if plain.encode_utf16().count() > SHELL_MAX_PATH {
        return Err(refuse(format!("{plain} is too long a path for the Recycle Bin: not deleting")));
    }
    if OsStr::new(&plain).to_str().is_none() || plain.contains('\u{fffd}') {
        return Err(refuse(format!("{plain}: name not representable for the shell: not deleting")));
    }
    Ok(OsStr::new(&plain).encode_wide().chain(Some(0)).collect())
}

/// Aborts any delete the shell would not send to the Recycle Bin.
#[implement(IFileOperationProgressSink)]
struct Guard(Arc<GuardState>);

#[derive(Default)]
struct GuardState {
    nuke: AtomicBool,
    flags: AtomicU32,
    result: AtomicU32,
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Guard_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hr: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, flags: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
        self.0.flags.store(flags, Ordering::SeqCst);
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            self.0.nuke.store(true, Ordering::SeqCst);
            return Err(E_ABORT.into());
        }
        Ok(())
    }
    fn PostDeleteItem(&self, _: u32, _: Ref<IShellItem>, hr: HRESULT, _: Ref<IShellItem>) -> windows::core::Result<()> {
        self.0.result.store(hr.0 as u32, Ordering::SeqCst);
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// An HRESULT as an `io::Error`, keeping Win32 codes (sharing violations).
fn hr_err(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(format!("Recycle Bin: {e}"))
    }
}

/// Recycles one item on a fresh STA thread.
fn recycle(wide: Vec<u16>) -> io::Result<()> {
    let t = std::thread::Builder::new().name("homeport-recycle".into()).spawn(move || -> io::Result<()> {
        // SAFETY: COM is initialized and uninitialized on this thread only.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok().map_err(hr_err)?;
        let r = (|| -> io::Result<()> {
            // SAFETY: COM calls on an STA thread with valid arguments.
            unsafe {
                let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(hr_err)?;
                let flags = FOF_ALLOWUNDO.0 | FOF_NO_UI.0 | FOFX_RECYCLEONDELETE.0 | FOFX_EARLYFAILURE.0;
                op.SetOperationFlags(FILEOPERATION_FLAGS(flags)).map_err(hr_err)?;
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(hr_err)?;
                let g = Arc::new(GuardState::default());
                let guard: IFileOperationProgressSink = Guard(g.clone()).into();
                op.DeleteItem(&item, &guard).map_err(hr_err)?;
                let performed = op.PerformOperations();
                tracing::debug!(flags = format!("{:#x}", g.flags.load(Ordering::SeqCst)), "PreDeleteItem");
                if g.nuke.load(Ordering::SeqCst) {
                    return Err(refuse(
                        "Windows would delete this item permanently instead of recycling it: not deleting".into(),
                    ));
                }
                performed.map_err(hr_err)?;
                let hr = g.result.load(Ordering::SeqCst);
                if hr & 0x8000_0000 != 0 {
                    return Err(hr_err(windows::core::Error::from_hresult(HRESULT(hr as i32))));
                }
                if op.GetAnyOperationsAborted().map_err(hr_err)?.as_bool() {
                    return Err(io::Error::other("the Recycle Bin operation was aborted"));
                }
            }
            Ok(())
        })();
        // SAFETY: paired with CoInitializeEx above.
        unsafe { CoUninitialize() };
        r
    })?;
    t.join().map_err(|_| io::Error::other("Recycle Bin thread panicked"))?
}

impl TrashOps for Trash {
    /// The bin is per user: homeportd must run as the user (it does, as a
    /// scheduled task in the user's name).
    fn open(_home: &Path, _data_dir: &Path) -> io::Result<Self> {
        Ok(Self)
    }

    fn trash(&self, dir: &Dir, name: &OsStr) -> io::Result<PathBuf> {
        let e = dir.stat(name)?;
        let path = {
            let h = dir.open_child(name, FILE_READ_ATTRIBUTES)?;
            final_path(h.as_raw_handle() as _)?
        };
        check_recyclable(&path, e.kind, e.size)?;
        let wide = shell_path(&path)?;
        busy_retry(|| recycle(wide.clone()))?;
        // Same volume, same FileId: find where it went.
        let h = Volume::open_key(dir.handle(), &e.key, FILE_READ_ATTRIBUTES).map_err(|err| {
            tracing::error!(path = %path.display(), error = %err, "recycled item not found in the Recycle Bin");
            err
        })?;
        let to = final_path(h.as_raw_handle() as _)?;
        if !self.contains(&to) {
            return Err(io::Error::other(format!(
                "{} was moved to {}, not the Recycle Bin",
                path.display(),
                to.display()
            )));
        }
        Ok(to)
    }

    /// Under `<drive>\$Recycle.Bin\`.
    fn contains(&self, path: &Path) -> bool {
        path.components().any(|c| matches!(c, Component::Normal(n) if n.eq_ignore_ascii_case("$Recycle.Bin")))
    }

    /// Renames the `$R…` item back (no replace) and removes its `$I…` record
    /// so Explorer stops listing it.
    fn put_back(&self, from: &Path, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
        let h = open_path(from, DELETE | FILE_READ_ATTRIBUTES)?;
        to_dir.rename_into(h.as_raw_handle() as _, to)?;
        drop(h);
        if let Some(n) = from.file_name().and_then(OsStr::to_str)
            && let Some(rest) = n.strip_prefix("$R")
        {
            let info = from.with_file_name(OsString::from(format!("$I{rest}")));
            if let Err(e) = std::fs::remove_file(&info) {
                tracing::debug!(path = %info.display(), error = %e, "no Recycle Bin record to remove");
            }
        }
        Ok(())
    }
}
