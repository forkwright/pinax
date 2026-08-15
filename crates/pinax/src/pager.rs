//! The pager: file-backed, checksummed, copy-on-write page storage
//! (PLAN.md Decision 1, Decision 2).
//!
//! WHY copy-on-write needs no WAL to be crash-safe here: a page id already
//! visible from the committed meta page is NEVER mutated in place — every
//! change targets a freshly allocated id (`allocate_page_id`), and the only
//! write that makes a change visible is [`Pager::commit`], which durably
//! writes the OTHER meta slot (ping-pong between page id 0 and 1) and
//! fsyncs twice: once for the data pages the new meta will reference, once
//! for the meta page itself. A crash at any point before the second fsync
//! leaves the previously-committed meta slot untouched and fully valid —
//! there is nothing to roll back because nothing reachable ever changed.
//! This is the mechanism ROADMAP.md Phase 01's "survive crash-and-reopen"
//! criterion demonstrates; Phase 02 adds the WAL for durability options
//! this scheme does not attempt (e.g. sub-transaction durability points).
//!
//! WHY page ids 0 and 1 (not `page_size`-relative offsets) always locate
//! the two meta slots: see [`crate::page::META_SLOT_LEN`] — bootstrap must
//! learn `page_size` from the meta page before it can compute anything
//! `page_size`-relative, so the meta region's own layout cannot depend on
//! that value.
//!
//! WHY no freelist reclamation in Phase 01: a page retired by this
//! transaction's copy-on-write is still referenced by the CURRENTLY ACTIVE
//! (pre-commit) meta until this transaction's meta write durably lands.
//! Overwriting that page's on-disk content before that point — the
//! prerequisite for a reusable freelist entry — would corrupt the
//! still-authoritative pre-transaction state if the process crashes in
//! between. Reusing an id retired by an EARLIER, already-committed
//! transaction is safe, but Phase 01's `page_count` is a pure bump
//! allocator that never looks at what a prior transaction retired — that is
//! real, deferred scope (tracked at forkwright/pinax, not silently
//! dropped), not a correctness gap: the database simply grows monotonically
//! rather than reusing space, which no Phase 01 acceptance criterion
//! (ROADMAP.md) requires reclaiming.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};

use snafu::{OptionExt as _, ResultExt as _, ensure};

use crate::codec::{read_u32, read_u64, write_u32, write_u64};
use crate::error::{
    AlreadyExistsSnafu, FatalError, FileTooSmallSnafu, IoSnafu, NoValidMetaPageSnafu, PinaxError,
    UnexpectedPageTypeSnafu,
};
use crate::page::{self, META_REGION_LEN, META_SLOT_LEN, PageSize, stamp_checksum};

const META_MAGIC: [u8; 4] = *b"PNX1";
const META_FORMAT_VERSION: u16 = 1;

const OFFSET_MAGIC: usize = 0;
const OFFSET_VERSION: usize = 4;
const OFFSET_PAGE_SIZE: usize = 6;
const OFFSET_TXN_ID: usize = 10;
const OFFSET_ROOT: usize = 18;
const OFFSET_PAGE_COUNT: usize = 22;
const OFFSET_FREELIST_HEAD: usize = 26;

/// The decoded content of one meta slot, once its checksum has verified.
struct MetaContent {
    page_size: PageSize,
    txn_id: u64,
    root_page_id: u32,
    page_count: u32,
}

/// Decode a meta slot's content, or `None` if its magic bytes or page-size
/// field are not well-formed.
///
/// WHY `Option` and not `Result`: the caller (`Pager::open`) only ever
/// wants to know "is this slot usable", already having verified the
/// checksum separately — a malformed magic or page size on a
/// checksum-valid slot cannot happen given `encode_meta_slot` is the only
/// writer, so folding both failure modes into "not usable" rather than a
/// typed error keeps `open`'s two-slot selection logic a plain match on
/// `Option`.
fn decode_meta_slot(buf: &[u8]) -> Option<MetaContent> {
    let magic = buf.get(OFFSET_MAGIC..OFFSET_MAGIC + 4)?;
    if magic != META_MAGIC {
        return None;
    }
    let page_size_raw = read_u32(buf, OFFSET_PAGE_SIZE).ok()?;
    let page_size = PageSize::try_from(page_size_raw).ok()?;
    let txn_id = read_u64(buf, OFFSET_TXN_ID).ok()?;
    let root_page_id = read_u32(buf, OFFSET_ROOT).ok()?;
    let page_count = read_u32(buf, OFFSET_PAGE_COUNT).ok()?;
    Some(MetaContent {
        page_size,
        txn_id,
        root_page_id,
        page_count,
    })
}

fn encode_meta_slot(content: &MetaContent) -> Result<Vec<u8>, PinaxError> {
    let slot_len = usize::try_from(META_SLOT_LEN).unwrap_or(4096);
    let mut buf = vec![0u8; slot_len];
    let magic_slot =
        buf.get_mut(OFFSET_MAGIC..OFFSET_MAGIC + 4)
            .context(crate::error::BufferBoundsSnafu {
                at: OFFSET_MAGIC,
                len: 4usize,
                buf_len: slot_len,
            })?;
    magic_slot.copy_from_slice(&META_MAGIC);
    crate::codec::write_u16(&mut buf, OFFSET_VERSION, META_FORMAT_VERSION)?;
    write_u32(&mut buf, OFFSET_PAGE_SIZE, content.page_size.bytes())?;
    write_u64(&mut buf, OFFSET_TXN_ID, content.txn_id)?;
    write_u32(&mut buf, OFFSET_ROOT, content.root_page_id)?;
    write_u32(&mut buf, OFFSET_PAGE_COUNT, content.page_count)?;
    write_u32(&mut buf, OFFSET_FREELIST_HEAD, 0)?;
    stamp_checksum(&mut buf)?;
    Ok(buf)
}

/// File-backed, checksummed, copy-on-write page storage.
///
/// WHY `pub(crate)`: `Database` (in `database.rs`) is the public entry
/// point; the pager is an implementation detail the buffer pool sits on
/// top of.
pub(crate) struct Pager {
    file: File,
    path: PathBuf,
    page_size: PageSize,
    active_slot: u8,
    txn_id: u64,
    root_page_id: u32,
    page_count: u32,
}

impl Pager {
    /// Create a fresh database file at `path` with the given `page_size`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::AlreadyExists`] if a file is
    /// already there, or [`FatalError::Io`] on any filesystem failure.
    pub(crate) fn create(path: &Path, page_size: PageSize) -> Result<Self, PinaxError> {
        let open_result = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path);
        let file = match open_result {
            Ok(file) => file,
            // WHY `?` in tail position rather than `return ....fail();`:
            // `.fail()`/`.context()` produce the LEAF error type
            // (`PermanentError`/`FatalError`), one level below this
            // function's declared `PinaxError` — `?` performs the `From`
            // conversion `#[snafu(transparent)]` provides; a bare `return`
            // would need that type to already match exactly.
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                AlreadyExistsSnafu {
                    path: path.to_path_buf(),
                }
                .fail()?
            }
            Err(source) => Err(source).context(IoSnafu {
                path: path.to_path_buf(),
            })?,
        };

        let content = MetaContent {
            page_size,
            txn_id: 0,
            root_page_id: page::EMPTY_TREE_ROOT,
            page_count: page::FIRST_DATA_PAGE_ID,
        };
        let slot = encode_meta_slot(&content)?;
        file.write_all_at(&slot, 0).context(IoSnafu {
            path: path.to_path_buf(),
        })?;
        file.sync_all().context(IoSnafu {
            path: path.to_path_buf(),
        })?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            page_size,
            active_slot: 0,
            txn_id: 0,
            root_page_id: page::EMPTY_TREE_ROOT,
            page_count: page::FIRST_DATA_PAGE_ID,
        })
    }

    /// Open an existing database file, picking whichever meta slot carries
    /// the higher verified `txn_id`.
    ///
    /// # Errors
    ///
    /// Returns [`FatalError::FileTooSmall`] if the file is shorter than the
    /// meta region, [`FatalError::NoValidMetaPage`] if neither slot's
    /// checksum verifies, or [`FatalError::Io`] on any filesystem failure.
    pub(crate) fn open(path: &Path) -> Result<Self, PinaxError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context(IoSnafu {
                path: path.to_path_buf(),
            })?;
        let actual_len = file
            .metadata()
            .context(IoSnafu {
                path: path.to_path_buf(),
            })?
            .len();
        ensure!(
            actual_len >= META_REGION_LEN,
            FileTooSmallSnafu {
                path: path.to_path_buf(),
                actual_len,
                min_len: META_REGION_LEN,
            }
        );

        let slot_len = usize::try_from(META_SLOT_LEN).unwrap_or(4096);
        let mut slot_a = vec![0u8; slot_len];
        let mut slot_b = vec![0u8; slot_len];
        file.read_exact_at(&mut slot_a, 0).context(IoSnafu {
            path: path.to_path_buf(),
        })?;
        file.read_exact_at(&mut slot_b, META_SLOT_LEN)
            .context(IoSnafu {
                path: path.to_path_buf(),
            })?;

        let valid_a = page::verify_checksum(&slot_a)
            .ok()
            .and_then(|()| decode_meta_slot(&slot_a));
        let valid_b = page::verify_checksum(&slot_b)
            .ok()
            .and_then(|()| decode_meta_slot(&slot_b));

        let (active_slot, content) = match (valid_a, valid_b) {
            (Some(a), Some(b)) if b.txn_id > a.txn_id => (1u8, b),
            (Some(a), Some(_)) => (0u8, a),
            (Some(a), None) => (0u8, a),
            (None, Some(b)) => (1u8, b),
            (None, None) => NoValidMetaPageSnafu {
                path: path.to_path_buf(),
            }
            .fail()?,
        };

        Ok(Self {
            file,
            path: path.to_path_buf(),
            page_size: content.page_size,
            active_slot,
            txn_id: content.txn_id,
            root_page_id: content.root_page_id,
            page_count: content.page_count,
        })
    }

    pub(crate) fn page_size(&self) -> PageSize {
        self.page_size
    }

    pub(crate) fn root_page_id(&self) -> u32 {
        self.root_page_id
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Byte offset of data page `id` within the file.
    fn file_offset(&self, id: u32) -> u64 {
        let index = u64::from(id.saturating_sub(page::FIRST_DATA_PAGE_ID));
        META_REGION_LEN + index * u64::from(self.page_size.bytes())
    }

    /// Read data page `id`, verifying its checksum.
    ///
    /// # Errors
    ///
    /// Returns [`FatalError::Corruption`] if the checksum does not verify,
    /// or [`FatalError::Io`] on any filesystem failure.
    pub(crate) fn read_data_page(&self, id: u32) -> Result<Vec<u8>, PinaxError> {
        let mut buf = vec![0u8; self.page_size.bytes_usize()];
        self.file
            .read_exact_at(&mut buf, self.file_offset(id))
            .context(IoSnafu {
                path: self.path.clone(),
            })?;
        page::verify_checksum(&buf).map_err(|(expected, actual)| PinaxError::Fatal {
            source: FatalError::Corruption {
                page_id: id,
                expected,
                actual,
                location: snafu::Location::new(file!(), line!(), column!()),
            },
        })?;
        Ok(buf)
    }

    /// Stamp `buf`'s checksum and write it to data page `id`.
    ///
    /// WHY no fsync per call: individual page writes durability is bounded
    /// by `sync_data`/`commit`, not by every write; batching the fsync per
    /// operation (rather than per page) is what makes group writes cheap.
    pub(crate) fn write_data_page(&self, id: u32, buf: &mut [u8]) -> Result<(), PinaxError> {
        stamp_checksum(buf)?;
        self.file
            .write_all_at(buf, self.file_offset(id))
            .context(IoSnafu {
                path: self.path.clone(),
            })
    }

    /// fsync data page writes. Called before a meta commit so the meta
    /// page a crash could observe never outruns the pages it references.
    pub(crate) fn sync_data(&self) -> Result<(), PinaxError> {
        self.file.sync_data().context(IoSnafu {
            path: self.path.clone(),
        })
    }

    /// Commit a new tree state: fsync data, write the inactive meta slot
    /// with an incremented `txn_id`, fsync again, then flip which slot is
    /// active in memory.
    ///
    /// # Errors
    ///
    /// Returns [`FatalError::Io`] on any filesystem failure. A failure here
    /// leaves the previously active slot as the durable state on reopen —
    /// see the module docs.
    pub(crate) fn commit(&mut self, new_root: u32, new_page_count: u32) -> Result<(), PinaxError> {
        self.sync_data()?;

        let target_slot = 1 - self.active_slot;
        let content = MetaContent {
            page_size: self.page_size,
            txn_id: self.txn_id + 1,
            root_page_id: new_root,
            page_count: new_page_count,
        };
        let slot_buf = encode_meta_slot(&content)?;
        let offset = u64::from(target_slot) * META_SLOT_LEN;
        self.file.write_all_at(&slot_buf, offset).context(IoSnafu {
            path: self.path.clone(),
        })?;
        self.file.sync_all().context(IoSnafu {
            path: self.path.clone(),
        })?;

        self.active_slot = target_slot;
        self.txn_id += 1;
        self.root_page_id = new_root;
        self.page_count = new_page_count;
        Ok(())
    }

    /// Verify a data page's `page_type` byte matches `expected`, returning
    /// [`FatalError::UnexpectedPageType`] otherwise.
    pub(crate) fn expect_page_type(
        id: u32,
        buf: &[u8],
        expected_byte: u8,
        expected_label: &'static str,
    ) -> Result<(), PinaxError> {
        let actual = crate::codec::read_u8(buf, 0)?;
        ensure!(
            actual == expected_byte,
            UnexpectedPageTypeSnafu {
                page_id: id,
                expected: expected_label,
                actual,
            }
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn create_then_open_round_trips_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        {
            let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
            assert_eq!(pager.root_page_id(), page::EMPTY_TREE_ROOT);
            assert_eq!(pager.page_count(), page::FIRST_DATA_PAGE_ID);
        }
        let reopened = Pager::open(&path).expect("open");
        assert_eq!(reopened.page_size().bytes(), 4096);
        assert_eq!(reopened.root_page_id(), page::EMPTY_TREE_ROOT);
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        Pager::create(&path, PageSize::DEFAULT).expect("first create");
        let err = Pager::create(&path, PageSize::DEFAULT).expect_err("second create must fail");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: crate::error::PermanentError::AlreadyExists { .. }
            }
        ));
    }

    #[test]
    fn open_missing_file_is_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "missing.pinax");
        let err = Pager::open(&path).expect_err("no such file");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::Io { .. }
            }
        ));
    }

    #[test]
    fn commit_persists_new_root_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        {
            let mut pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
            pager
                .commit(page::FIRST_DATA_PAGE_ID, page::FIRST_DATA_PAGE_ID + 1)
                .expect("commit");
            assert_eq!(pager.root_page_id(), page::FIRST_DATA_PAGE_ID);
        }
        let reopened = Pager::open(&path).expect("reopen");
        assert_eq!(reopened.root_page_id(), page::FIRST_DATA_PAGE_ID);
        assert_eq!(reopened.page_count(), page::FIRST_DATA_PAGE_ID + 1);
    }

    #[test]
    fn commit_alternates_meta_slots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        let mut pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        assert_eq!(pager.active_slot, 0);
        pager
            .commit(page::EMPTY_TREE_ROOT, page::FIRST_DATA_PAGE_ID)
            .expect("commit 1");
        assert_eq!(pager.active_slot, 1);
        pager
            .commit(page::EMPTY_TREE_ROOT, page::FIRST_DATA_PAGE_ID)
            .expect("commit 2");
        assert_eq!(pager.active_slot, 0);
    }

    #[test]
    fn crash_before_commit_leaves_prior_state_on_reopen() {
        // WHY this models a crash: writing a data page (as any in-flight
        // operation does before it calls `commit`) without ever calling
        // `commit` is exactly what a process crash mid-operation leaves
        // behind — the meta slot on disk never advances past the prior
        // txn_id. This is ROADMAP.md Phase 01's "survive crash-and-reopen"
        // criterion at the pager layer (`database.rs` tests exercise it
        // end-to-end through real B+tree mutations).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        let mut orphan = vec![0xAA_u8; pager.page_size().bytes_usize()];
        pager
            .write_data_page(page::FIRST_DATA_PAGE_ID, &mut orphan)
            .expect("write survives even without commit");
        drop(pager); // no commit() call: models an uncommitted crash

        let reopened = Pager::open(&path).expect("reopen after simulated crash");
        assert_eq!(reopened.root_page_id(), page::EMPTY_TREE_ROOT);
        assert_eq!(reopened.page_count(), page::FIRST_DATA_PAGE_ID);
    }

    #[test]
    fn corrupted_data_page_fails_checksum_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        let mut pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        let mut buf = vec![0x11_u8; pager.page_size().bytes_usize()];
        pager
            .write_data_page(page::FIRST_DATA_PAGE_ID, &mut buf)
            .expect("write");
        pager
            .commit(page::FIRST_DATA_PAGE_ID, page::FIRST_DATA_PAGE_ID + 1)
            .expect("commit");

        // Flip one byte directly on disk, bypassing the pager (simulates
        // storage-level bit rot).
        let offset = pager.file_offset(page::FIRST_DATA_PAGE_ID);
        let mut byte = [0u8; 1];
        pager
            .file
            .read_exact_at(&mut byte, offset)
            .expect("read byte");
        byte[0] ^= 0xFF;
        pager.file.write_all_at(&byte, offset).expect("flip byte");

        let err = pager
            .read_data_page(page::FIRST_DATA_PAGE_ID)
            .expect_err("checksum must catch the flipped byte");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::Corruption { .. }
            }
        ));
    }

    #[test]
    fn corrupted_active_meta_slot_falls_back_to_prior_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        {
            let mut pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
            pager
                .commit(page::FIRST_DATA_PAGE_ID, page::FIRST_DATA_PAGE_ID + 1)
                .expect("commit txn 1, now active slot 1");
        }
        // Corrupt slot 1 (the currently-active slot) directly.
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for corruption");
        let mut byte = [0u8; 1];
        file.read_exact_at(&mut byte, META_SLOT_LEN)
            .expect("read byte of slot 1");
        byte[0] ^= 0xFF;
        file.write_all_at(&byte, META_SLOT_LEN)
            .expect("flip byte in slot 1");

        let reopened = Pager::open(&path).expect("falls back to slot 0");
        assert_eq!(reopened.root_page_id(), page::EMPTY_TREE_ROOT);
        assert_eq!(reopened.txn_id, 0);
    }

    #[test]
    fn both_meta_slots_corrupted_is_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_db_path(&dir, "db.pinax");
        Pager::create(&path, PageSize::DEFAULT).expect("create");
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for corruption");
        let zeros = vec![0u8; usize::try_from(META_REGION_LEN).unwrap_or(8192)];
        file.write_all_at(&zeros, 0)
            .expect("zero the whole meta region");

        let err = Pager::open(&path).expect_err("neither slot verifies");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::NoValidMetaPage { .. }
            }
        ));
    }

    #[test]
    fn expect_page_type_accepts_match_and_rejects_mismatch() {
        let mut buf = vec![0u8; 16];
        crate::codec::write_u8(&mut buf, 0, page::PAGE_TYPE_LEAF).expect("in bounds");
        Pager::expect_page_type(2, &buf, page::PAGE_TYPE_LEAF, "leaf").expect("matches");
        let err = Pager::expect_page_type(2, &buf, page::PAGE_TYPE_INTERIOR, "interior")
            .expect_err("byte is leaf, not interior");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::UnexpectedPageType { .. }
            }
        ));
    }
}
