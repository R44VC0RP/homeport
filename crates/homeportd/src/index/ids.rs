//! Item identity: Homeport's own stable ids, with the OS file key as a hint.
//!
//! - [`IdScheme::Direct`] (macOS): `id = (slot << 56) | key.id`, where `slot`
//!   numbers the volumes shares live on (persisted by stable volume id, the
//!   first volume is slot 0). On a schema-2 database this gives exactly the
//!   APFS file ids clients already hold. Keys it cannot encode (an unknown
//!   volume, a file id ≥ 2⁵⁶) fall back to assigned ids from 2⁶².
//! - [`IdScheme::Assigned`] (Linux, Windows): ids come from a counter and the
//!   `ids` table maps key → id. The index carries an id over to a new key
//!   when a save replaces the file (see `Index::reconcile`).
//!
//! Recent assignments are cached in memory ahead of the database writer, so
//! lookups never wait for a commit.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

use crate::db::{Db, DbError, Op};
use crate::platform::{FileKey, IdScheme, Volume, VolumeOps};

const SLOT_SHIFT: u32 = 56;
const DIRECT_MASK: u64 = (1 << SLOT_SHIFT) - 1;
/// First assigned id under the direct scheme.
const DIRECT_ASSIGNED_BASE: u64 = 1 << 62;
const CACHE: usize = 1 << 16;

struct Assigned {
    next: u64,
    by_id: LruCache<u64, FileKey>,
    by_key: LruCache<FileKey, u64>,
}

pub struct Ids {
    scheme: IdScheme,
    db: std::sync::Arc<Db>,
    /// Stable volume ids by slot (persisted); with their runtime volumes.
    slots: Mutex<Vec<(String, Option<Volume>)>>,
    assigned: Mutex<Assigned>,
}

fn poisoned<T>(_: T) -> DbError {
    DbError::Poisoned
}

impl Ids {
    pub fn new(scheme: IdScheme, db: std::sync::Arc<Db>, vol_slots: Vec<String>, next_id: u64) -> Self {
        let base = match scheme {
            IdScheme::Direct => DIRECT_ASSIGNED_BASE,
            IdScheme::Assigned => 1,
        };
        let cap = NonZeroUsize::new(CACHE).unwrap_or(NonZeroUsize::MIN);
        Self {
            scheme,
            db,
            slots: Mutex::new(vol_slots.into_iter().map(|s| (s, None)).collect()),
            assigned: Mutex::new(Assigned {
                next: next_id.max(base),
                by_id: LruCache::new(cap),
                by_key: LruCache::new(cap),
            }),
        }
    }

    pub fn scheme(&self) -> IdScheme {
        self.scheme
    }

    /// Makes a share's volume known (and gives it a slot under the direct scheme).
    pub fn register_volume(&self, vol: Volume) -> Result<(), DbError> {
        let stable = vol.stable_id();
        let mut slots = self.slots.lock().map_err(poisoned)?;
        match slots.iter_mut().find(|(s, _)| *s == stable) {
            Some(slot) => slot.1 = Some(vol),
            None => {
                slots.push((stable, Some(vol)));
                let value = slots.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join("\n");
                self.db.send(Op::Meta { key: "vol_slots", value })?;
            }
        }
        Ok(())
    }

    /// The registered volume holding `key`.
    pub fn with_volume<T>(&self, key: &FileKey, f: impl FnOnce(&Volume) -> T) -> Option<T> {
        let slots = self.slots.lock().ok()?;
        slots.iter().filter_map(|(_, v)| v.as_ref()).find(|v| v.key() == key.vol).map(f)
    }

    fn direct_encode(&self, key: &FileKey) -> Option<u64> {
        let id = u64::try_from(key.id).ok().filter(|&i| i <= DIRECT_MASK)?;
        let slots = self.slots.lock().ok()?;
        let slot = slots.iter().position(|(_, v)| v.as_ref().is_some_and(|v| v.key() == key.vol))?;
        (slot < 64).then_some(((slot as u64) << SLOT_SHIFT) | id)
    }

    /// Whether the direct scheme can encode `key` (else it is on an unknown volume).
    pub fn encodable(&self, key: &FileKey) -> bool {
        self.scheme == IdScheme::Assigned || self.direct_encode(key).is_some()
    }

    /// The id of a key, if it has one (never assigns).
    pub fn lookup(&self, key: &FileKey) -> Result<Option<u64>, DbError> {
        if self.scheme == IdScheme::Direct
            && let Some(id) = self.direct_encode(key)
        {
            return Ok(Some(id));
        }
        if let Some(&id) = self.assigned.lock().map_err(poisoned)?.by_key.get(key) {
            return Ok(Some(id));
        }
        let id = self.db.id_of_key(key)?;
        if let Some(id) = id {
            let mut a = self.assigned.lock().map_err(poisoned)?;
            a.by_key.put(*key, id);
            a.by_id.put(id, *key);
        }
        Ok(id)
    }

    /// The id of a key, assigning a new one if needed.
    pub fn get_or_assign(&self, key: &FileKey) -> Result<u64, DbError> {
        if let Some(id) = self.lookup(key)? {
            return Ok(id);
        }
        let mut a = self.assigned.lock().map_err(poisoned)?;
        let id = a.next;
        a.next += 1;
        a.by_key.put(*key, id);
        a.by_id.put(id, *key);
        self.db.send(Op::IdSet { id, key: *key })?;
        self.db.send(Op::Meta { key: "next_id", value: a.next.to_string() })?;
        Ok(id)
    }

    /// `id` now names `key` (a save replaced the file).
    pub fn repoint(&self, id: u64, key: &FileKey) -> Result<(), DbError> {
        if self.scheme == IdScheme::Direct && id < DIRECT_ASSIGNED_BASE {
            return Ok(());
        }
        let mut a = self.assigned.lock().map_err(poisoned)?;
        if let Some(old) = a.by_id.put(id, *key) {
            a.by_key.pop(&old);
        }
        a.by_key.put(*key, id);
        self.db.send(Op::IdSet { id, key: *key })
    }

    /// The key an id names, if known.
    pub fn key_of(&self, id: u64) -> Result<Option<FileKey>, DbError> {
        if self.scheme == IdScheme::Direct && id < DIRECT_ASSIGNED_BASE {
            let slot = (id >> SLOT_SHIFT) as usize;
            let slots = self.slots.lock().map_err(poisoned)?;
            let Some(vol) = slots.get(slot).and_then(|(_, v)| v.as_ref()) else { return Ok(None) };
            return Ok(Some(FileKey { vol: vol.key(), id: u128::from(id & DIRECT_MASK), generation: 0 }));
        }
        if let Some(&k) = self.assigned.lock().map_err(poisoned)?.by_id.get(&id) {
            return Ok(Some(k));
        }
        let key = self.db.key_of_id(id)?;
        if let Some(k) = key {
            let mut a = self.assigned.lock().map_err(poisoned)?;
            a.by_id.put(id, k);
            a.by_key.put(k, id);
        }
        Ok(key)
    }
}
