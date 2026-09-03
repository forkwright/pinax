//! Error types for pinax's pager, buffer pool, and B+tree.
//!
//! WHY the three-tier shape: PLAN.md Decision 13 fixes `PinaxError` as
//! `Transient | Permanent | Fatal` at the public boundary so a consumer can
//! dispatch retry logic on the outer shape without string-matching a
//! display message. Phase 01 has no locking, no WAL, and no MVCC, so no
//! condition in this phase is legitimately retryable — Decision 13's own
//! examples for `Transient` (`Busy`, `WriteWriteConflict`, `Checkpoint`) are
//! all lock-contention, MVCC-validation, and WAL conditions that Phase
//! 02/03 introduce. An empty `Transient` variant would be dead code with
//! nothing to construct it, so it is added when a phase first produces one
//! rather than reserved empty now.

use std::path::PathBuf;

/// Errors raised by pinax's page format, pager, buffer pool, and B+tree.
///
/// WHY `#[non_exhaustive]`: both inner enums grow as later phases add
/// conditions (WAL, MVCC, encryption); a caller matching today must not
/// break when Phase 02 adds `Transient::Busy`.
///
/// WHY `#[snafu(transparent)]` on both variants: it gives each inner enum
/// (`PermanentError`, `FatalError`) an auto-generated `From` conversion
/// into `PinaxError`, so every call site below can write
/// `.context(SomeLeafSnafu { .. })?` against the LEAF enum's own
/// context selector and let `?` lift it through `PinaxError` in one step,
/// rather than every fallible call needing two explicit `.context()` hops.
#[derive(Debug, snafu::Snafu)]
#[non_exhaustive]
pub enum PinaxError {
    /// A caller-correctable condition: bad input, a violated CRUD
    /// precondition (key exists / key missing), a bad configuration value.
    #[snafu(transparent)]
    Permanent {
        /// The specific permanent condition.
        source: PermanentError,
    },
    /// A condition that means the database file, a page, or the process's
    /// invariants can no longer be trusted: checksum failure, I/O failure,
    /// or an internal invariant violation.
    #[snafu(transparent)]
    Fatal {
        /// The specific fatal condition.
        source: FatalError,
    },
}

/// Caller-correctable errors: bad input or a violated CRUD precondition.
#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub(crate)))]
#[non_exhaustive]
pub enum PermanentError {
    /// A page size outside the locked valid set (Decision 2: 4096 / 8192 /
    /// 16384 / 32768 / 65536).
    #[snafu(display(
        "page size {requested} is not one of the valid sizes 4096/8192/16384/32768/65536"
    ))]
    InvalidPageSize {
        /// The rejected page-size value.
        requested: u32,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A buffer pool was constructed with zero capacity.
    #[snafu(display("buffer pool capacity must be at least 1 page"))]
    InvalidBufferPoolCapacity {
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// `Database::create` was called against a path that already contains a
    /// file.
    #[snafu(display("database file already exists at {path:?}; use Database::open"))]
    AlreadyExists {
        /// The path that already existed.
        path: PathBuf,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// `insert` was called with a key already present in the tree.
    #[snafu(display("key {key} already exists; use update"))]
    KeyAlreadyExists {
        /// The colliding integer key.
        key: i64,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// `update` or `delete` was called with a key absent from the tree.
    #[snafu(display("key {key} does not exist"))]
    KeyNotFound {
        /// The missing integer key.
        key: i64,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A row's encoded byte length does not fit in the page format's `u32`
    /// payload-length field.
    #[snafu(display("row for key {key} encodes to {encoded_len} bytes, exceeding u32::MAX"))]
    PayloadTooLarge {
        /// The row's key.
        key: i64,
        /// The row's encoded length.
        encoded_len: usize,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Errors that mean the database file, a page, or an internal invariant can
/// no longer be trusted.
#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub(crate)))]
#[non_exhaustive]
pub enum FatalError {
    /// A page's stored checksum did not match its recomputed checksum
    /// (Decision 2: XxHash3-64 in the trailing 8 reserved bytes).
    #[snafu(display(
        "page {page_id} failed checksum verification: expected {expected:016x}, got {actual:016x}"
    ))]
    Corruption {
        /// The page whose checksum did not verify.
        page_id: u32,
        /// The checksum stored in the page's trailing reserved bytes.
        expected: u64,
        /// The checksum recomputed from the page's content.
        actual: u64,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// Neither meta slot (page 0 nor page 1) verified its checksum on open.
    ///
    /// WHY distinct from `Corruption`: a single bad page identifies which
    /// page is wrong. Both meta slots failing means the file is not a
    /// readable pinax database at all (never created, truncated, or wrong
    /// file entirely) rather than one corrupt page inside an otherwise
    /// valid one.
    #[snafu(display("no valid meta page found in {path:?} (checked slots 0 and 1)"))]
    NoValidMetaPage {
        /// The database file path.
        path: PathBuf,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// The database file was shorter than the fixed meta-page region.
    #[snafu(display("{path:?} is {actual_len} bytes, shorter than the meta region ({min_len})"))]
    FileTooSmall {
        /// The database file path.
        path: PathBuf,
        /// The file's actual length in bytes.
        actual_len: u64,
        /// The minimum length a readable pinax file must have.
        min_len: u64,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A page byte offset fell outside the buffer being read or written.
    ///
    /// WHY this exists rather than a panic: every byte access in `codec`
    /// goes through `.get()`/`.get_mut()` (no indexing) precisely so a
    /// malformed or corrupt page produces this typed error instead of a
    /// panic. Reaching this variant on a page whose checksum verified is an
    /// internal encoding bug — the codec and the encoders that call it are
    /// expected to keep every offset in bounds by construction.
    #[snafu(display("byte range [{at}, {at}+{len}) is out of bounds for a {buf_len}-byte buffer"))]
    BufferBounds {
        /// The offset the access started at.
        at: usize,
        /// The number of bytes the access needed.
        len: usize,
        /// The buffer's actual length.
        buf_len: usize,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A page was read whose `page_type` byte did not match any known
    /// variant, or did not match the type the caller expected at that
    /// position in the tree.
    #[snafu(display("page {page_id}: expected page type {expected}, got byte {actual}"))]
    UnexpectedPageType {
        /// The page whose type byte was wrong.
        page_id: u32,
        /// The page type the caller expected (as a debug label).
        expected: &'static str,
        /// The raw type byte actually stored on the page.
        actual: u8,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A row's byte payload, read from a page that already passed checksum
    /// verification, did not decode as a value of the on-disk row format.
    ///
    /// WHY this is `Fatal` and not `Permanent`: a checksum-valid page's
    /// bytes came from `Row::encode`, which only ever writes tags and
    /// lengths this crate's own decoder understands. Reaching this variant
    /// means the checksum passed over bytes the decoder still cannot
    /// parse — either an encode/decode mismatch bug, or corruption that
    /// happened to preserve the checksum (astronomically unlikely for
    /// XxHash3-64, but not the caller's mistake to correct either way).
    #[snafu(display("row payload failed to decode: {reason}"))]
    InvalidRowEncoding {
        /// What specifically failed to decode.
        reason: &'static str,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// The buffer pool's LRU recency queue was empty while eviction was
    /// still required to satisfy a capacity bound.
    ///
    /// WHY this cannot happen by construction, and is still handled: the
    /// pool never removes an entry from `entries` without also removing it
    /// from `recency` in the same operation (INVARIANT enforced by
    /// `BufferPool::put_new`/`evict_one`). Surfacing this as a typed error
    /// rather than a panic keeps the "no unwrap/expect" rule intact if that
    /// invariant is ever violated by a future edit.
    #[snafu(display("buffer pool recency queue underflowed capacity accounting"))]
    PoolInvariantViolated {
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// An I/O operation against the database file failed.
    #[snafu(display("I/O error on {path:?}: {source}"))]
    Io {
        /// The database file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_display_wraps_source() {
        let err = PinaxError::Permanent {
            source: PermanentError::KeyNotFound {
                key: 7,
                location: snafu::location!(),
            },
        };
        assert_eq!(err.to_string(), "key 7 does not exist");
    }

    #[test]
    fn fatal_display_wraps_source() {
        let err = PinaxError::Fatal {
            source: FatalError::Corruption {
                page_id: 3,
                expected: 1,
                actual: 2,
                location: snafu::location!(),
            },
        };
        assert!(err.to_string().contains("page 3 failed checksum"));
    }
}
