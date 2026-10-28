//! FSEvents over raw CoreServices FFI.

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

pub mod flags {
    pub const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    pub const USER_DROPPED: u32 = 0x0000_0002;
    pub const KERNEL_DROPPED: u32 = 0x0000_0004;
    pub const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
    pub const HISTORY_DONE: u32 = 0x0000_0010;
    pub const ROOT_CHANGED: u32 = 0x0000_0020;
}

const CREATE_NO_DEFER: u32 = 0x0000_0002;
const CREATE_WATCH_ROOT: u32 = 0x0000_0004;
const CREATE_FILE_EVENTS: u32 = 0x0000_0010;
const SINCE_NOW: u64 = u64::MAX;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

type CFRef = *const c_void;
type Callback = extern "C" fn(CFRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFRef>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(alloc: CFRef, s: *const c_char, enc: u32) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, n: isize, cb: *const c_void) -> CFRef;
    fn CFUUIDCreateString(alloc: CFRef, uuid: CFRef) -> CFRef;
    fn CFStringGetCString(s: CFRef, buf: *mut c_char, len: isize, enc: u32) -> u8;
    fn CFRelease(r: CFRef);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: CFRef,
        cb: Callback,
        ctx: *const StreamContext,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(s: *mut c_void, q: *mut c_void);
    fn FSEventStreamStart(s: *mut c_void) -> u8;
    fn FSEventStreamStop(s: *mut c_void);
    fn FSEventStreamInvalidate(s: *mut c_void);
    fn FSEventStreamRelease(s: *mut c_void);
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
    fn FSEventsGetCurrentEventId() -> u64;
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

#[derive(Debug, thiserror::Error)]
pub enum FsEventsError {
    #[error("FSEventStreamCreate failed")]
    Create,
    #[error("FSEventStreamStart failed")]
    Start,
    #[error("path contains NUL")]
    Path,
}

/// One delivered event: path, flags, event ID.
pub type Event = (PathBuf, u32, u64);
/// A callback's events, tagged with the stream they came from.
pub type Batch = (u8, Vec<Event>);

struct Info {
    tag: u8,
    tx: mpsc::UnboundedSender<Batch>,
}

/// Running stream; stops it on drop.
pub struct Watcher {
    stream: *mut c_void,
    sender: *mut Info,
}

// SAFETY: the stream is only touched again in `drop`, which FSEvents allows
// from any thread; the sender box is only freed after the stream is invalidated.
unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: `stream` came from FSEventStreamCreate and was started.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.sender));
        }
    }
}

extern "C" fn callback(_s: CFRef, info: *mut c_void, n: usize, paths: *mut c_void, fl: *const u32, ids: *const u64) {
    // SAFETY: FSEvents passes `n` C strings, flags and IDs; `info` is our sender.
    unsafe {
        let info = &*(info as *const Info);
        let paths = paths as *const *const c_char;
        let mut batch = Vec::with_capacity(n);
        for i in 0..n {
            let p = CStr::from_ptr(*paths.add(i));
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(p.to_bytes()));
            batch.push((path, *fl.add(i), *ids.add(i)));
        }
        let _ = info.tx.send((info.tag, batch));
    }
}

/// UUID of the FSEvents database for a device; event IDs are only valid with it.
pub fn volume_uuid(dev: libc::dev_t) -> Option<String> {
    // SAFETY: CoreFoundation calls with owned references released below.
    unsafe {
        let uuid = FSEventsCopyUUIDForDevice(dev);
        if uuid.is_null() {
            return None;
        }
        let s = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid);
        if s.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 64];
        let ok = CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, CF_STRING_ENCODING_UTF8);
        CFRelease(s);
        (ok != 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

pub fn current_event_id() -> u64 {
    // SAFETY: no preconditions.
    unsafe { FSEventsGetCurrentEventId() }
}

/// Watches `paths` with file-level events, replaying from `since` when given.
/// Batches go to `tx` tagged with `tag`.
pub fn watch(
    paths: &[&Path],
    since: Option<u64>,
    latency: f64,
    tag: u8,
    tx: mpsc::UnboundedSender<Batch>,
) -> Result<Watcher, FsEventsError> {
    let sender = Box::into_raw(Box::new(Info { tag, tx }));
    // SAFETY: CoreFoundation objects are created and released here; the
    // context's `info` outlives the stream (freed in `Watcher::drop`).
    unsafe {
        let mut cfstrs = Vec::new();
        for p in paths {
            let c = CString::new(p.as_os_str().as_bytes()).map_err(|_| FsEventsError::Path)?;
            cfstrs.push(CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8));
        }
        let arr = CFArrayCreate(
            std::ptr::null(),
            cfstrs.as_ptr(),
            cfstrs.len() as isize,
            (&raw const kCFTypeArrayCallBacks).cast(),
        );
        for s in cfstrs {
            CFRelease(s);
        }
        let ctx =
            StreamContext { version: 0, info: sender.cast(), retain: None, release: None, copy_description: None };
        let stream = FSEventStreamCreate(
            std::ptr::null(),
            callback,
            &ctx,
            arr,
            since.unwrap_or(SINCE_NOW),
            latency,
            CREATE_NO_DEFER | CREATE_FILE_EVENTS | CREATE_WATCH_ROOT,
        );
        CFRelease(arr);
        if stream.is_null() {
            drop(Box::from_raw(sender));
            return Err(FsEventsError::Create);
        }
        let queue = dispatch_queue_create(c"dev.vogel.homeportd.fsevents".as_ptr(), std::ptr::null());
        FSEventStreamSetDispatchQueue(stream, queue);
        if FSEventStreamStart(stream) == 0 {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
            drop(Box::from_raw(sender));
            return Err(FsEventsError::Start);
        }
        Ok(Watcher { stream, sender })
    }
}
