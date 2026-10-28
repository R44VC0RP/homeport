//! `ReadDirectoryChangesExW` as a [`WatcherOps`]: one recursive watch per
//! share root on its own thread. Events become `Change::Path` hints (renames
//! give both names); a lost batch (buffer overflow) becomes `Change::All`.
//!
//! There is no persistent cursor: reading the USN journal needs an elevated
//! token on Windows 10 (`FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` is Windows 11).
//! On start the watcher asks for a rescan of every indexed directory, which
//! costs about one listing per indexed directory.

use std::ffi::OsString;
use std::io;
use std::mem::offset_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_EXTENDED_INFORMATION, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesExW, ReadDirectoryNotifyExtendedInformation,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

use super::fs::long_path;
use crate::platform::{Change, WatchBatch, WatcherOps};

/// Notification buffer per root. Local volumes accept more than 64 KiB.
const BUF_BYTES: usize = 1 << 20;

struct Root {
    dir: Arc<OwnedHandle>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct Watcher {
    roots: Vec<Root>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for r in &mut self.roots {
            r.stop.store(true, Ordering::SeqCst);
            // SAFETY: the handle is open; cancels the pending read.
            unsafe { CancelIoEx(r.dir.as_raw_handle() as HANDLE, std::ptr::null()) };
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
        }
    }
}

fn open_dir(root: &Path) -> io::Result<OwnedHandle> {
    let p = long_path(root)?;
    // SAFETY: `p` is NUL-terminated.
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: freshly opened.
    Ok(unsafe { <OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(h) })
}

/// Parses one buffer of `FILE_NOTIFY_EXTENDED_INFORMATION` records.
fn parse(root: &Path, buf: &[u8], out: &mut Vec<Change>) {
    let name_off = offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName);
    let mut off = 0usize;
    while off + name_off <= buf.len() {
        // SAFETY: bounds checked; records are DWORD-aligned in an 8-aligned buffer.
        let info = unsafe { &*buf.as_ptr().add(off).cast::<FILE_NOTIFY_EXTENDED_INFORMATION>() };
        let n = info.FileNameLength as usize / 2;
        if off + name_off + n * 2 > buf.len() {
            break;
        }
        // SAFETY: bounds checked above.
        let name = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(off + name_off).cast::<u16>(), n) };
        let rel = PathBuf::from(OsString::from_wide(name));
        // Every action (including both names of a rename) is a hint for the
        // path and its parent; the index rescans what it has indexed.
        out.push(Change::Path(root.join(rel)));
        if info.NextEntryOffset == 0 {
            break;
        }
        off += info.NextEntryOffset as usize;
    }
}

fn run(root: PathBuf, dir: Arc<OwnedHandle>, stop: Arc<AtomicBool>, tx: mpsc::UnboundedSender<WatchBatch>) {
    let h = dir.as_raw_handle() as HANDLE;
    let mut buf = vec![0u64; BUF_BYTES / 8];
    // SAFETY: plain manual-reset event.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        tracing::error!(root = %root.display(), error = %io::Error::last_os_error(), "watcher: CreateEvent failed");
        return;
    }
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION;
    let mut failures = 0u32;
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: zeroed OVERLAPPED with our event.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event;
        // SAFETY: `buf` and `ov` outlive the I/O: we wait for it below before reusing either.
        let ok = unsafe {
            ReadDirectoryChangesExW(
                h,
                buf.as_mut_ptr().cast(),
                BUF_BYTES as u32,
                1,
                filter,
                null_mut(),
                &mut ov,
                None,
                ReadDirectoryNotifyExtendedInformation,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            failures += 1;
            tracing::warn!(root = %root.display(), error = %e, "ReadDirectoryChangesExW failed");
            let _ = tx.send(WatchBatch { changes: vec![Change::All], cursor: None });
            if failures > 5 {
                tracing::error!(root = %root.display(), "watcher stopped");
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        // SAFETY: `event` is valid.
        if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
            break;
        }
        let mut n = 0u32;
        // SAFETY: the I/O completed (event signalled).
        let done = unsafe { GetOverlappedResult(h, &ov, &mut n, 0) };
        let mut changes = Vec::new();
        if done == 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(c) if c == ERROR_OPERATION_ABORTED as i32 => break,
                Some(c) if c == ERROR_NOTIFY_ENUM_DIR as i32 => changes.push(Change::All),
                _ => {
                    tracing::warn!(root = %root.display(), error = %e, "watcher read failed");
                    changes.push(Change::All);
                    failures += 1;
                    if failures > 5 {
                        break;
                    }
                }
            }
        } else if n == 0 {
            // Too many changes for the buffer: they were dropped.
            changes.push(Change::All);
        } else {
            failures = 0;
            // SAFETY: the first `n` bytes were written by the call.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
            parse(&root, bytes, &mut changes);
        }
        if !changes.is_empty() && tx.send(WatchBatch { changes, cursor: None }).is_err() {
            break;
        }
    }
    // SAFETY: our event.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(event) };
}

impl WatcherOps for Watcher {
    fn start(roots: &[PathBuf], cursor: Option<&str>, tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        let mut out = Vec::new();
        for root in roots {
            let dir = Arc::new(open_dir(root)?);
            let stop = Arc::new(AtomicBool::new(false));
            let (r, d, s, t) = (root.clone(), dir.clone(), stop.clone(), tx.clone());
            let thread = std::thread::Builder::new().name("homeport-watch".into()).spawn(move || run(r, d, s, t))?;
            out.push(Root { dir, stop, thread: Some(thread) });
        }
        tracing::info!(roots = roots.len(), "watching with ReadDirectoryChangesExW");
        // Changes while the server was down are unknown: rescan what is indexed
        // (the watches are already up, so nothing falls in between).
        let _ = cursor;
        let _ = tx.send(WatchBatch { changes: vec![Change::All], cursor: Some("rdcw".into()) });
        Ok(Self { roots: out })
    }

    /// Recursive per root: nothing to do.
    fn watch_dir(&self, _path: &Path) {}

    fn unwatch_dir(&self, _path: &Path) {}
}
