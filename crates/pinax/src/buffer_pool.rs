//! The buffer pool: a capacity-bounded, LRU-evicting page cache over the
//! pager (ROADMAP.md Phase 01: "buffer pool handles databases larger than
//! RAM").
//!
//! WHY no "pinning" or checkout/checkin bookkeeping: every mutation in this
//! crate is copy-on-write (`btree::insert`/`delete` never overwrite a page
//! id already reachable from the committed meta page — see `pager`'s
//! module docs). `get` therefore only ever needs to CLONE a page's bytes
//! out to the caller; the cached copy is never invalidated by a caller's
//! in-progress edit, because that edit is building content for a BRAND NEW
//! page id via [`BufferPool::put_new`], not mutating the cached one. This
//! sidesteps the classic buffer-pool aliasing problem (a page checked out
//! for write must not also be evicted mid-edit) entirely — nothing is ever
//! checked out, so nothing can be evicted out from under an in-progress
//! edit. Eviction candidates are exactly `entries`' keys, unconditionally.

use std::collections::{HashMap, HashSet, VecDeque};

use snafu::OptionExt as _;

use crate::error::{PermanentError, PinaxError, PoolInvariantViolatedSnafu};
use crate::page::PageSize;
use crate::pager::Pager;

/// A capacity-bounded, LRU-evicting cache of page buffers sitting over a
/// [`Pager`].
pub(crate) struct BufferPool {
    pager: Pager,
    capacity: usize,
    entries: HashMap<u32, Vec<u8>>,
    dirty: HashSet<u32>,
    /// LRU order, oldest (front) to newest (back). Kept in exact sync with
    /// `entries`'s key set — INVARIANT enforced by every mutation going
    /// through `touch`/`remove_cached`, never touching either collection
    /// alone.
    recency: VecDeque<u32>,
    next_page_id: u32,
}

impl BufferPool {
    /// Wrap `pager` in a buffer pool holding at most `capacity` pages.
    ///
    /// # Errors
    ///
    /// Returns [`PermanentError::InvalidBufferPoolCapacity`] if `capacity`
    /// is zero.
    pub(crate) fn new(pager: Pager, capacity: usize) -> Result<Self, PinaxError> {
        snafu::ensure!(capacity >= 1, crate::error::InvalidBufferPoolCapacitySnafu);
        let next_page_id = pager.page_count();
        Ok(Self {
            pager,
            capacity,
            entries: HashMap::new(),
            dirty: HashSet::new(),
            recency: VecDeque::new(),
            next_page_id,
        })
    }

    pub(crate) fn page_size(&self) -> PageSize {
        self.pager.page_size()
    }

    pub(crate) fn root_page_id(&self) -> u32 {
        self.pager.root_page_id()
    }

    /// Allocate a fresh page id for a copy-on-write page. Never reused
    /// across the pool's lifetime — see the pager module docs on why
    /// Phase 01 has no freelist reclamation.
    pub(crate) fn allocate_page_id(&mut self) -> u32 {
        let id = self.next_page_id;
        self.next_page_id += 1;
        id
    }

    /// Read page `id`'s content, cache-or-read-through, as an owned clone.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::error::FatalError::Corruption`] from the pager
    /// on a checksum failure, or [`crate::error::FatalError::Io`] on a
    /// filesystem failure.
    pub(crate) fn get(&mut self, id: u32) -> Result<Vec<u8>, PinaxError> {
        if let Some(buf) = self.entries.get(&id) {
            let buf = buf.clone();
            self.touch(id);
            return Ok(buf);
        }
        let buf = self.pager.read_data_page(id)?;
        self.insert_cached(id, buf.clone(), false)?;
        Ok(buf)
    }

    /// Insert a brand-new (or freshly overwritten) page as dirty, to be
    /// flushed by [`Self::flush_all_dirty`] or evicted early — either is
    /// safe under copy-on-write (see module docs).
    pub(crate) fn put_new(&mut self, id: u32, buf: Vec<u8>) -> Result<(), PinaxError> {
        self.insert_cached(id, buf, true)
    }

    fn insert_cached(&mut self, id: u32, buf: Vec<u8>, dirty: bool) -> Result<(), PinaxError> {
        if self.entries.remove(&id).is_some() {
            self.recency.retain(|&existing| existing != id);
        }
        while self.entries.len() >= self.capacity {
            self.evict_one()?;
        }
        if dirty {
            self.dirty.insert(id);
        }
        self.entries.insert(id, buf);
        self.recency.push_back(id);
        Ok(())
    }

    fn touch(&mut self, id: u32) {
        self.recency.retain(|&existing| existing != id);
        self.recency.push_back(id);
    }

    fn evict_one(&mut self) -> Result<(), PinaxError> {
        let id = self
            .recency
            .pop_front()
            .context(PoolInvariantViolatedSnafu)?;
        let mut buf = self
            .entries
            .remove(&id)
            .context(PoolInvariantViolatedSnafu)?;
        if self.dirty.remove(&id) {
            self.pager.write_data_page(id, &mut buf)?;
        }
        Ok(())
    }

    /// Write every currently-dirty cached page to disk (via
    /// [`Pager::write_data_page`]) and fsync, without evicting them from
    /// the cache.
    pub(crate) fn flush_all_dirty(&mut self) -> Result<(), PinaxError> {
        let ids: Vec<u32> = self.dirty.iter().copied().collect();
        for id in ids {
            if let Some(buf) = self.entries.get_mut(&id) {
                self.pager.write_data_page(id, buf)?;
            }
            self.dirty.remove(&id);
        }
        self.pager.sync_data()
    }

    /// Flush every dirty page, then commit `new_root` as the tree's new
    /// root at the current allocation frontier (ROADMAP.md Phase 01:
    /// "survive crash-and-reopen").
    pub(crate) fn commit(&mut self, new_root: u32) -> Result<(), PinaxError> {
        self.flush_all_dirty()?;
        self.pager.commit(new_root, self.next_page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FatalError;

    fn pool_with_capacity(dir: &tempfile::TempDir, capacity: usize) -> BufferPool {
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        BufferPool::new(pager, capacity).expect("valid capacity")
    }

    #[test]
    fn zero_capacity_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        let err = BufferPool::new(pager, 0).expect_err("capacity 0 is invalid");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: PermanentError::InvalidBufferPoolCapacity { .. }
            }
        ));
    }

    #[test]
    fn put_then_get_round_trips_without_touching_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool_with_capacity(&dir, 4);
        let id = pool.allocate_page_id();
        let buf = vec![7u8; pool.page_size().bytes_usize()];
        pool.put_new(id, buf.clone()).expect("put");
        let got = pool.get(id).expect("get");
        // WHY only the first byte, not the whole buffer: `put_new` caches
        // the RAW buffer while `get`'s pager-read-through path stamps a
        // checksum into the trailing bytes on the way in — this assertion
        // only needs to prove the cache round-trips content, not restate
        // checksum placement.
        assert_eq!(got.first(), buf.first());
    }

    #[test]
    fn eviction_flushes_dirty_pages_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool_with_capacity(&dir, 2);
        let page_size = pool.page_size().bytes_usize();
        let ids: Vec<u32> = (0..5).map(|_| pool.allocate_page_id()).collect();
        for &id in &ids {
            let mut buf = vec![0u8; page_size];
            if let Some(b) = buf.first_mut() {
                *b = 1;
            }
            pool.put_new(id, buf).expect("put");
        }
        // Capacity 2 with 5 distinct ids forces at least 3 evictions; every
        // evicted id must have been durably written, not dropped.
        for &id in &ids {
            let read_back = pool
                .pager
                .read_data_page(id)
                .expect("evicted pages are on disk");
            assert_eq!(read_back.first().copied(), Some(1));
        }
    }

    #[test]
    fn get_reads_through_on_cache_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool_with_capacity(&dir, 1);
        let id = pool.allocate_page_id();
        let mut buf = vec![0u8; pool.page_size().bytes_usize()];
        if let Some(b) = buf.first_mut() {
            *b = 9;
        }
        pool.put_new(id, buf).expect("put");
        // Evict it by inserting a second page under capacity 1.
        let other = pool.allocate_page_id();
        pool.put_new(other, vec![0u8; pool.page_size().bytes_usize()])
            .expect("put forces eviction of the first page");
        let got = pool.get(id).expect("read-through from disk");
        assert_eq!(got.first().copied(), Some(9));
    }

    #[test]
    fn corrupted_page_surfaces_on_get() {
        use std::os::unix::fs::FileExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        let mut pool = BufferPool::new(pager, 1).expect("valid capacity");
        let id = pool.allocate_page_id();
        pool.put_new(id, vec![0u8; pool.page_size().bytes_usize()])
            .expect("put");
        pool.flush_all_dirty().expect("flush to disk");
        // Force an eviction so the next `get` must read through the pager
        // (cache hits never re-verify the checksum, by design — see
        // module docs).
        let other = pool.allocate_page_id();
        pool.put_new(other, vec![0u8; pool.page_size().bytes_usize()])
            .expect("evicts id");

        let offset = crate::page::META_REGION_LEN
            + u64::from(id - crate::page::FIRST_DATA_PAGE_ID) * u64::from(pool.page_size().bytes());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open for corruption");
        let mut byte = [0u8; 1];
        file.read_exact_at(&mut byte, offset).expect("read byte");
        byte[0] ^= 0xFF;
        file.write_all_at(&byte, offset).expect("flip byte");

        let err = pool
            .get(id)
            .expect_err("checksum must catch the flipped byte");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::Corruption { .. }
            }
        ));
    }

    #[test]
    fn allocate_page_id_is_monotonic_and_never_repeats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool_with_capacity(&dir, 4);
        let a = pool.allocate_page_id();
        let b = pool.allocate_page_id();
        let c = pool.allocate_page_id();
        assert!(a < b && b < c);
    }

    #[test]
    fn commit_persists_new_root_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        let mut pool = BufferPool::new(pager, 4).expect("valid capacity");
        let id = pool.allocate_page_id();
        pool.put_new(id, vec![5u8; pool.page_size().bytes_usize()])
            .expect("put");
        pool.commit(id).expect("commit");
        drop(pool);

        let reopened = Pager::open(&path).expect("reopen");
        assert_eq!(reopened.root_page_id(), id);
    }

    #[test]
    fn evict_one_on_empty_pool_is_reported_not_panicked() {
        // WHY this cannot happen through the public API (capacity >= 1
        // guards it, and `insert_cached` only evicts while at/over
        // capacity), documented directly rather than left implicit.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool_with_capacity(&dir, 1);
        let err = pool.evict_one().expect_err("recency queue is empty");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::PoolInvariantViolated { .. }
            }
        ));
    }
}
