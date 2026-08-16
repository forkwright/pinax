//! [`Database`]: the public Phase 01 facade over the pager, buffer pool,
//! and B+tree (ROADMAP.md Phase 01: "open a file, CRUD rows by integer
//! key, survive crash-and-reopen").
//!
//! WHY one implicit tree per file rather than a multi-table catalog:
//! PLAN.md Decision 1 fixes "one file per database"; a named-table catalog
//! (`CREATE TABLE`) is Phase 04 territory (ROADMAP.md), which is expected
//! to layer a table-name-to-root-page-id registry on top of the same
//! pager/buffer-pool/B+tree engine this phase lands. Every `Database`
//! today owns exactly one anonymous, integer-keyed B+tree.

use std::path::Path;

use crate::btree;
use crate::buffer_pool::BufferPool;
use crate::error::PinaxError;
use crate::page::PageSize;
use crate::pager::Pager;
use crate::row::Row;

/// Default buffer pool capacity in pages: 256 pages (1 MiB at the default
/// 4 KiB page size). Callers with a memory budget smaller than their
/// working set should use [`Database::create_with_capacity`] /
/// [`Database::open_with_capacity`] instead — see those docs and
/// ROADMAP.md Phase 01's "buffer pool handles databases larger than RAM"
/// criterion.
pub const DEFAULT_BUFFER_POOL_CAPACITY: usize = 256;

/// A pinax database file: one B+tree, keyed by `i64`, storing
/// [`lexis::Value`] tuple rows (Decision 1, Decision 5).
pub struct Database {
    pool: BufferPool,
}

impl Database {
    /// Create a fresh database file at `path` with `page_size` and the
    /// default buffer pool capacity.
    ///
    /// # Errors
    ///
    /// See [`Self::create_with_capacity`].
    pub fn create(path: &Path, page_size: PageSize) -> Result<Self, PinaxError> {
        Self::create_with_capacity(path, page_size, DEFAULT_BUFFER_POOL_CAPACITY)
    }

    /// Create a fresh database file at `path` with an explicit buffer pool
    /// capacity (in pages).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::AlreadyExists`] if a file
    /// is already at `path`, [`crate::error::PermanentError::InvalidBufferPoolCapacity`]
    /// if `capacity` is zero, or [`crate::error::FatalError::Io`] on any
    /// filesystem failure.
    pub fn create_with_capacity(
        path: &Path,
        page_size: PageSize,
        capacity: usize,
    ) -> Result<Self, PinaxError> {
        let pager = Pager::create(path, page_size)?;
        let pool = BufferPool::new(pager, capacity)?;
        Ok(Self { pool })
    }

    /// Open an existing database file with the default buffer pool
    /// capacity.
    ///
    /// # Errors
    ///
    /// See [`Self::open_with_capacity`].
    pub fn open(path: &Path) -> Result<Self, PinaxError> {
        Self::open_with_capacity(path, DEFAULT_BUFFER_POOL_CAPACITY)
    }

    /// Open an existing database file with an explicit buffer pool
    /// capacity (in pages) — deliberately small relative to the on-disk
    /// size demonstrates ROADMAP.md Phase 01's "buffer pool handles
    /// databases larger than RAM" criterion.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::FatalError::FileTooSmall`] or
    /// [`crate::error::FatalError::NoValidMetaPage`] if `path` is not a
    /// readable pinax database, [`crate::error::PermanentError::InvalidBufferPoolCapacity`]
    /// if `capacity` is zero, or [`crate::error::FatalError::Io`] on any
    /// filesystem failure.
    pub fn open_with_capacity(path: &Path, capacity: usize) -> Result<Self, PinaxError> {
        let pager = Pager::open(path)?;
        let pool = BufferPool::new(pager, capacity)?;
        Ok(Self { pool })
    }

    /// Insert `row` under `key`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::KeyAlreadyExists`] if `key`
    /// is already present, or a [`crate::error::FatalError`] variant on
    /// I/O or corruption.
    pub fn insert(&mut self, key: i64, row: &Row) -> Result<(), PinaxError> {
        btree::insert(&mut self.pool, key, row)?;
        Ok(())
    }

    /// Look up the row stored at `key`. `Ok(None)` if absent — a missing
    /// key is not an error condition (Decision 13's classification
    /// reserves error variants for conditions the caller must act on).
    ///
    /// # Errors
    ///
    /// Returns a [`crate::error::FatalError`] variant on I/O or
    /// corruption.
    pub fn get(&mut self, key: i64) -> Result<Option<Row>, PinaxError> {
        btree::get(&mut self.pool, key)
    }

    /// Replace the row stored at `key`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::KeyNotFound`] if `key` is
    /// absent, or a [`crate::error::FatalError`] variant on I/O or
    /// corruption.
    pub fn update(&mut self, key: i64, row: &Row) -> Result<(), PinaxError> {
        btree::update(&mut self.pool, key, row)?;
        Ok(())
    }

    /// Delete and return the row stored at `key`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::KeyNotFound`] if `key` is
    /// absent, or a [`crate::error::FatalError`] variant on I/O or
    /// corruption.
    pub fn delete(&mut self, key: i64) -> Result<Row, PinaxError> {
        let (_new_root, row) = btree::delete(&mut self.pool, key)?;
        Ok(row)
    }

    /// Every `(key, row)` pair in ascending key order.
    ///
    /// # Errors
    ///
    /// Returns a [`crate::error::FatalError`] variant on I/O or
    /// corruption.
    pub fn scan(&mut self) -> Result<Vec<(i64, Row)>, PinaxError> {
        btree::scan(&mut self.pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lexis::Value;

    fn row(n: i64) -> Row {
        Row::new(vec![Value::Integer(n)])
    }

    // ROADMAP.md Phase 01 criterion: "open a file, CRUD rows by integer
    // key".
    #[test]
    fn full_crud_cycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        let mut db = Database::create(&path, PageSize::DEFAULT).expect("create");

        db.insert(1, &row(1)).expect("insert");
        assert_eq!(db.get(1).expect("get").expect("present"), row(1));

        db.update(1, &row(2)).expect("update");
        assert_eq!(db.get(1).expect("get").expect("present"), row(2));

        let removed = db.delete(1).expect("delete");
        assert_eq!(removed, row(2));
        assert_eq!(db.get(1).expect("get"), None);
    }

    #[test]
    fn create_then_open_reads_back_inserted_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        {
            let mut db = Database::create(&path, PageSize::DEFAULT).expect("create");
            db.insert(1, &row(1)).expect("insert");
            db.insert(2, &row(2)).expect("insert");
        }
        let mut reopened = Database::open(&path).expect("open");
        assert_eq!(reopened.get(1).expect("get").expect("present"), row(1));
        assert_eq!(reopened.get(2).expect("get").expect("present"), row(2));
    }

    #[test]
    fn open_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.pinax");
        assert!(Database::open(&path).is_err());
    }

    #[test]
    fn scan_returns_rows_in_ascending_key_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        let mut db = Database::create(&path, PageSize::DEFAULT).expect("create");
        for k in [5, 1, 3, 2, 4] {
            db.insert(k, &row(k)).expect("insert");
        }
        let scanned: Vec<i64> = db
            .scan()
            .expect("scan")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(scanned, vec![1, 2, 3, 4, 5]);
    }
}
