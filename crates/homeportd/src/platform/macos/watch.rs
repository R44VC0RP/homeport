//! FSEvents as a [`WatcherOps`]: a live stream from now plus, when resuming,
//! a history stream that replays what happened while the server was down.
//! They are separate because FSEvents holds live events back until a replay
//! finishes, which can take seconds.
//!
//! The cursor is `<volume UUIDs, comma-separated>:<event id>`; it is only
//! handed out from the live stream once the replay is done, so a crash
//! mid-replay replays again.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::sync::mpsc;

use super::fs::Volume;
use super::fsevents::{self, flags};
use crate::platform::{Change, VolumeOps, WatchBatch, WatcherOps};

pub struct Watcher {
    _live: fsevents::Watcher,
    _history: std::sync::Arc<Mutex<Option<fsevents::Watcher>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn volumes_id(roots: &[PathBuf]) -> Option<String> {
    let mut ids: Vec<String> = Vec::new();
    for r in roots {
        let v = Volume::of(r).ok()?;
        let id = fsevents::volume_uuid(v.dev())?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Some(ids.join(","))
}

impl WatcherOps for Watcher {
    fn start(roots: &[PathBuf], cursor: Option<&str>, tx: mpsc::UnboundedSender<WatchBatch>) -> io::Result<Self> {
        let vols = volumes_id(roots);
        let since = match (cursor.and_then(|c| c.rsplit_once(':')), &vols) {
            (Some((v, id)), Some(cur)) if v == cur => id.parse::<u64>().ok(),
            _ => None,
        };
        let paths: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let (ev_tx, mut events) = mpsc::unbounded_channel();
        let err = |e: fsevents::FsEventsError| io::Error::other(e.to_string());
        let live = fsevents::watch(&paths, None, 0.1, 0, ev_tx.clone()).map_err(err)?;
        let history = match since {
            Some(id) => Some(fsevents::watch(&paths, Some(id), 0.1, 1, ev_tx).map_err(err)?),
            None => None,
        };
        tracing::info!(resume_from = ?since, volume = ?vols, "watching FSEvents");
        let vols = vols.unwrap_or_default();
        if since.is_none() {
            // Nothing to replay: remember where we start.
            let cursor = format!("{vols}:{}", fsevents::current_event_id());
            let _ = tx.send(WatchBatch { changes: Vec::new(), cursor: Some(cursor) });
        }
        let history = std::sync::Arc::new(Mutex::new(history));
        let hist = history.clone();
        let task = tokio::spawn(async move {
            while let Some((tag, mut batch)) = events.recv().await {
                let mut history_done = tag == 1 && batch.iter().any(|e| e.1 & flags::HISTORY_DONE != 0);
                while let Ok((t, more)) = events.try_recv() {
                    history_done |= t == 1 && more.iter().any(|e| e.1 & flags::HISTORY_DONE != 0);
                    batch.extend(more);
                }
                let replaying = {
                    let mut h = hist.lock().unwrap_or_else(|p| p.into_inner());
                    if history_done && h.take().is_some() {
                        tracing::info!("FSEvents history replay done");
                    }
                    h.is_some()
                };
                let max_id = batch.iter().map(|e| e.2).max().unwrap_or(0);
                if batch.iter().any(|e| e.1 & flags::EVENT_IDS_WRAPPED != 0) {
                    tracing::warn!("FSEvents event IDs wrapped");
                }
                if batch.iter().any(|e| e.1 & flags::ROOT_CHANGED != 0) {
                    tracing::warn!("a share root moved or was deleted");
                }
                let mut changes = Vec::with_capacity(batch.len());
                for (path, f, _) in batch {
                    if f & flags::HISTORY_DONE != 0 {
                        continue;
                    }
                    if f & (flags::USER_DROPPED | flags::KERNEL_DROPPED) != 0 {
                        changes.push(Change::All);
                    }
                    if f & flags::MUST_SCAN_SUBDIRS != 0 {
                        changes.push(Change::Subtree(path.clone()));
                    }
                    changes.push(Change::Path(path));
                }
                let cursor = (max_id > 0 && !replaying).then(|| format!("{vols}:{max_id}"));
                if tx.send(WatchBatch { changes, cursor }).is_err() {
                    return;
                }
            }
        });
        Ok(Self { _live: live, _history: history, task })
    }

    /// FSEvents watches whole trees.
    fn watch_dir(&self, _path: &Path) {}

    fn unwatch_dir(&self, _path: &Path) {}
}
