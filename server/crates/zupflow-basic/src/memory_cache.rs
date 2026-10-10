//! [`MemoryCache`]: an in-process cache, bounded in entries, in bytes and in
//! the size of one value, so it can't grow without limit. An expiry index
//! keeps eviction at O(log n): when full it drops what expires soonest, which
//! is anything already expired first.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::storage::{Cache, StorageError};

type Slot = (Scope, Name);

struct Entry {
    value: Vec<u8>,
    /// The entry's key in the expiry index.
    expiry: (Instant, u64),
}

#[derive(Default)]
struct Entries {
    by_slot: HashMap<Slot, Entry>,
    by_expiry: BTreeMap<(Instant, u64), Slot>,
    bytes: usize,
    next: u64,
}

impl Entries {
    fn remove(&mut self, slot: &Slot) {
        if let Some(entry) = self.by_slot.remove(slot) {
            self.by_expiry.remove(&entry.expiry);
            self.bytes -= entry.value.len();
        }
    }

    fn insert(&mut self, slot: Slot, value: Vec<u8>, expires_at: Instant) {
        self.remove(&slot);
        self.next += 1;
        let expiry = (expires_at, self.next);
        self.bytes += value.len();
        self.by_expiry.insert(expiry, slot.clone());
        self.by_slot.insert(slot, Entry { value, expiry });
    }

    /// Drops the entry closest to expiring. False when there's nothing left.
    fn evict_soonest(&mut self) -> bool {
        match self.by_expiry.pop_first() {
            Some((_, slot)) => {
                if let Some(entry) = self.by_slot.remove(&slot) {
                    self.bytes -= entry.value.len();
                }
                true
            }
            None => false,
        }
    }
}

pub struct MemoryCache {
    entries: Mutex<Entries>,
    max_entries: usize,
    max_bytes: usize,
    max_value_bytes: usize,
}

impl MemoryCache {
    pub fn new(max_entries: usize, max_bytes: usize, max_value_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(Entries::default()),
            max_entries: max_entries.max(1),
            max_bytes,
            max_value_bytes: max_value_bytes.min(max_bytes),
        }
    }

    fn entries(&self) -> MutexGuard<'_, Entries> {
        // A panic mid-update may have left the map half-written. Everything in a
        // cache can be rebuilt, so start over instead of failing forever.
        self.entries.lock().unwrap_or_else(|poisoned| {
            let mut entries = poisoned.into_inner();
            *entries = Entries::default();
            // Recovered once: without this every later call would start over again.
            self.entries.clear_poison();
            entries
        })
    }
}

/// When an entry written `now` with `ttl` expires. A TTL too large to
/// represent means "as long as anything lives here".
fn expiry(now: Instant, ttl: Duration) -> Instant {
    now.checked_add(ttl)
        .or_else(|| now.checked_add(Duration::from_secs(10 * 365 * 24 * 3600)))
        .unwrap_or(now)
}

impl MemoryCache {
    /// Writes an entry under the held lock, then evicts down to the bounds.
    fn store(&self, entries: &mut Entries, slot: Slot, value: Vec<u8>, expires_at: Instant) {
        entries.insert(slot, value, expires_at);
        while entries.by_slot.len() > self.max_entries || entries.bytes > self.max_bytes {
            if !entries.evict_soonest() {
                break;
            }
        }
    }
}

impl Default for MemoryCache {
    /// 10,000 entries and 64 MiB in all, at most 1 MiB per value.
    fn default() -> Self {
        Self::new(10_000, 64 << 20, 1 << 20)
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, scope: &Scope, key: &Name) -> Result<Option<Vec<u8>>, StorageError> {
        let mut entries = self.entries();
        let slot = (*scope, key.clone());
        let Some(entry) = entries.by_slot.get(&slot) else {
            return Ok(None);
        };
        if entry.expiry.0 > Instant::now() {
            return Ok(Some(entry.value.clone()));
        }
        entries.remove(&slot);
        Ok(None)
    }

    async fn put(&self, scope: &Scope, key: &Name, value: Vec<u8>, ttl: Duration) -> Result<(), StorageError> {
        if value.len() > self.max_value_bytes {
            return Err(StorageError::TooLarge);
        }
        let expires_at = expiry(Instant::now(), ttl);
        let mut entries = self.entries();
        self.store(&mut entries, (*scope, key.clone()), value, expires_at);
        Ok(())
    }

    async fn add_if_absent(
        &self,
        scope: &Scope,
        key: &Name,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool, StorageError> {
        if value.len() > self.max_value_bytes {
            return Err(StorageError::TooLarge);
        }
        let now = Instant::now();
        let slot = (*scope, key.clone());
        // One lock for the check and the write: that is what makes it atomic.
        let mut entries = self.entries();
        if entries.by_slot.get(&slot).is_some_and(|entry| entry.expiry.0 > now) {
            return Ok(false);
        }
        self.store(&mut entries, slot, value, expiry(now, ttl));
        Ok(true)
    }

    async fn delete(&self, scope: &Scope, key: &Name) -> Result<(), StorageError> {
        self.entries().remove(&(*scope, key.clone()));
        Ok(())
    }

    async fn delete_scope(&self, scope: &Scope) -> Result<(), StorageError> {
        let mut entries = self.entries();
        let slots: Vec<Slot> = entries
            .by_slot
            .keys()
            .filter(|(entry_scope, _)| entry_scope == scope)
            .cloned()
            .collect();
        for slot in slots {
            entries.remove(&slot);
        }
        Ok(())
    }
}
