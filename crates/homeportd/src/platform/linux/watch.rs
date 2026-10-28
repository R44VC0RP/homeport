//! inotify on indexed directories only (inotify is not recursive, and
//! watching only what clients have listed keeps `node_modules` free until
//! someone opens it). There is no resumable cursor: every start rescans the
//! indexed directories (`Change::All`) and the index diffs them.

use std::collections::{HashMap, HashSet};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use tokio::sync::mpsc;

use super::fs::err;
use crate::platform::{Change, WatchBatch, WatcherOps};

const MAX_WATCHES: &str = "/proc/sys/fs/inotify/max_user_watches";
/// After the first event, wait this long for more before sending a batch...
const QUIET: Duration = Duration::from_millis(30);
/// ...but never longer than this.
const WINDOW: Duration = Duration::from_millis(100);
/// `watch_dir` calls this soon after `start` come from the index registering
/// the directories it already has; the initial `Change::All` covers them.
const ARM_AFTER: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Maps {
    by_wd: HashMap<i32, PathBuf>,
    by_path: HashMap<PathBuf, i32>,
}

struct Inner {
    fd: OwnedFd,
    maps: Mutex<Maps>,
    tx: mpsc::UnboundedSender<WatchBatch>,
    stop: AtomicBool,
    full_warned: AtomicBool,
    started: Instant,
}

pub struct Watcher {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn max_watches() -> Option<u64> {
    std::fs::read_to_string(MAX_WATCHES).ok()?.trim().parse().ok()
}

impl Inner {
    fn maps(&self) -> std::sync::MutexGuard<'_, Maps> {
        self.maps.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Turns one read's worth of events into hints.
    fn read_events(&self, buf: &mut [MaybeUninit<u8>], out: &mut Vec<Change>) -> io::Result<bool> {
        let mut reader = inotify::Reader::new(&self.fd, buf);
        let mut any = false;
        loop {
            let ev = match reader.next() {
                Ok(ev) => ev,
                Err(rustix::io::Errno::AGAIN) => return Ok(any),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(err(e)),
            };
            any = true;
            let flags = ev.events();
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                tracing::warn!("inotify queue overflowed; rescanning every indexed directory");
                out.push(Change::All);
                continue;
            }
            let mut maps = self.maps();
            let Some(dir) = maps.by_wd.get(&ev.wd()).cloned() else { continue };
            if flags.contains(ReadFlags::IGNORED) {
                // The directory is gone (or unwatched): forget the watch.
                maps.by_wd.remove(&ev.wd());
                if maps.by_path.get(&dir) == Some(&ev.wd()) {
                    maps.by_path.remove(&dir);
                }
            }
            drop(maps);
            match ev.file_name().filter(|n| !n.is_empty()) {
                Some(name) => out.push(Change::Path(dir.join(std::ffi::OsStr::from_bytes(name.to_bytes())))),
                // DELETE_SELF, MOVE_SELF, IGNORED, UNMOUNT, ATTRIB on the directory.
                None => out.push(Change::Path(dir)),
            }
        }
    }

    fn run(&self) {
        let mut buf = vec![MaybeUninit::<u8>::uninit(); 64 * 1024];
        let mut pending: Vec<Change> = Vec::new();
        let mut first: Option<Instant> = None;
        while !self.stop.load(Ordering::Relaxed) {
            let timeout = match first {
                Some(_) => rustix::time::Timespec { tv_sec: 0, tv_nsec: QUIET.as_nanos() as _ },
                None => rustix::time::Timespec { tv_sec: 0, tv_nsec: 250_000_000 },
            };
            let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
            let ready = match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => {
                    tracing::error!(error = %e, "inotify poll failed; watching stopped");
                    let _ = self.tx.send(WatchBatch { changes: vec![Change::All], cursor: None });
                    return;
                }
            };
            if ready {
                match self.read_events(&mut buf, &mut pending) {
                    Ok(true) if first.is_none() => first = Some(Instant::now()),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "inotify read failed"),
                }
            }
            let due = first.is_some_and(|t| !ready || t.elapsed() >= WINDOW);
            if due && !pending.is_empty() {
                let changes = dedupe(std::mem::take(&mut pending));
                if self.tx.send(WatchBatch { changes, cursor: None }).is_err() {
                    return;
                }
                first = None;
            } else if due {
                first = None;
            }
        }
    }
}

/// Drops repeated hints (a burst of writes to one file is one rescan).
fn dedupe(changes: Vec<Change>) -> Vec<Change> {
    if changes.contains(&Change::All) {
        return vec![Change::All];
    }
    let (mut paths, mut trees) = (HashSet::new(), HashSet::new());
    changes
        .into_iter()
        .filter(|c| match c {
            Change::Path(p) => paths.insert(p.clone()),
            Change::Subtree(p) => trees.insert(p.clone()),
            Change::All => true,
        })
        .collect()
}

impl WatcherOps for Watcher {
    fn start(_roots: &[PathBuf], _cursor: Option<&str>, tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).map_err(err)?;
        match max_watches() {
            Some(n) => tracing::info!(
                max_user_watches = n,
                "watching indexed directories with inotify (one watch each; raise fs.inotify.max_user_watches if it runs out)"
            ),
            None => tracing::info!("watching indexed directories with inotify"),
        }
        // No cursor: rescan everything the index has and diff.
        let _ = tx.send(WatchBatch { changes: vec![Change::All], cursor: None });
        let inner = Arc::new(Inner {
            fd,
            maps: Mutex::new(Maps::default()),
            tx,
            stop: AtomicBool::new(false),
            full_warned: AtomicBool::new(false),
            started: Instant::now(),
        });
        let i = inner.clone();
        let thread = std::thread::Builder::new().name("inotify".into()).spawn(move || i.run())?;
        Ok(Self { inner, thread: Some(thread) })
    }

    /// Adds a watch, then (once past startup) asks for a rescan of the
    /// directory, which catches what changed between its scan and the watch.
    fn watch_dir(&self, path: &Path) {
        let flags = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::ATTRIB
            | WatchFlags::MODIFY
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR
            | WatchFlags::DONT_FOLLOW
            | WatchFlags::EXCL_UNLINK;
        let wd = match inotify::add_watch(&self.inner.fd, path, flags) {
            Ok(wd) => wd,
            Err(rustix::io::Errno::NOSPC) => {
                if !self.inner.full_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        path = %path.display(),
                        max_user_watches = ?max_watches(),
                        "out of inotify watches: changes in some directories will be missed; raise it with \
                         `sudo sysctl fs.inotify.max_user_watches=1048576` (and in /etc/sysctl.d)"
                    );
                }
                return;
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "inotify_add_watch");
                return;
            }
        };
        let fresh = {
            let mut maps = self.inner.maps();
            // The same directory under a new path (it moved): one watch, new path.
            if let Some(old) = maps.by_wd.insert(wd, path.to_owned())
                && old != path
                && maps.by_path.get(&old) == Some(&wd)
            {
                maps.by_path.remove(&old);
            }
            maps.by_path.insert(path.to_owned(), wd) != Some(wd)
        };
        if fresh && self.inner.started.elapsed() >= ARM_AFTER {
            let _ = self.inner.tx.send(WatchBatch { changes: vec![Change::Subtree(path.to_owned())], cursor: None });
        }
    }

    fn unwatch_dir(&self, path: &Path) {
        let wd = self.inner.maps().by_path.remove(path);
        if let Some(wd) = wd {
            // The IGNORED event that follows removes `by_wd`.
            let _ = inotify::remove_watch(&self.inner.fd, wd);
        }
    }
}
