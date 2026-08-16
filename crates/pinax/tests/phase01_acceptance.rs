//! Phase 01 acceptance tests, one per ROADMAP.md criterion, verbatim:
//!
//! - "Open a file, CRUD rows by integer key, survive crash-and-reopen"
//! - "Page format checksums verified; corruption detected"
//! - "Buffer pool handles databases larger than RAM"
//!
//! WHY a dedicated integration-test file rather than folding these into
//! each module's colocated unit tests: the conformance bar's default is
//! colocated `#[cfg(test)] mod tests` (and every module here has that, for
//! its own internal behavior) — this file exists so the five acceptance
//! criteria stay traceable as a named, standalone set rather than
//! scattered evidence a reader has to reassemble from module-level tests.

// WHY `expect`/`expect_err` throughout rather than `?`: this file is a
// separate crate root (every file under `tests/` is), so it does not
// inherit `lib.rs`'s `#![cfg_attr(test, allow(clippy::unwrap_used,
// clippy::expect_used))]` escape that covers colocated `#[cfg(test)] mod
// tests` blocks. The workspace `[workspace.lints.clippy]` already sets
// `expect_used` to "warn", not "deny", specifically because "tests
// legitimately use these" (`Cargo.toml`) — this crate-level `expect`
// restates that same intent at the one scope the gate's `-D warnings`
// cannot see through. `unwrap_used` is not listed: this file has no bare
// `.unwrap()` call, and an expectation nothing fires against is itself a
// gate error (`unfulfilled_lint_expectations`).
#![expect(
    clippy::expect_used,
    reason = "acceptance tests use expect/expect_err as the intended failure mode, matching every other test surface in this workspace"
)]

use std::fs::OpenOptions;
use std::os::unix::fs::FileExt as _;

use lexis::Value;
use pinax::{Database, PageSize, PinaxError};

fn sample_row(n: i64) -> pinax::Row {
    pinax::Row::new(vec![
        Value::Integer(n),
        Value::Text(format!("acceptance-row-{n}")),
    ])
}

/// ROADMAP.md Phase 01: "Open a file, CRUD rows by integer key".
#[test]
fn open_a_file_and_crud_rows_by_integer_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crud.pinax");

    // Open (create) a file.
    let mut db = Database::create(&path, PageSize::DEFAULT).expect("open a file");

    // Create.
    db.insert(1, &sample_row(1)).expect("create row 1");
    db.insert(2, &sample_row(2)).expect("create row 2");

    // Read.
    assert_eq!(db.get(1).expect("read row 1"), Some(sample_row(1)));
    assert_eq!(db.get(2).expect("read row 2"), Some(sample_row(2)));
    assert_eq!(db.get(3).expect("read missing row"), None);

    // Update.
    db.update(1, &sample_row(100)).expect("update row 1");
    assert_eq!(
        db.get(1).expect("read updated row 1"),
        Some(sample_row(100))
    );

    // Delete.
    let deleted = db.delete(2).expect("delete row 2");
    assert_eq!(deleted, sample_row(2));
    assert_eq!(db.get(2).expect("read deleted row"), None);

    // Row 1 (updated, not deleted) is still there.
    assert_eq!(db.get(1).expect("read row 1 again"), Some(sample_row(100)));
}

/// ROADMAP.md Phase 01: "survive crash-and-reopen".
///
/// Simulates a crash by writing committed data, then dropping the
/// `Database` WITHOUT any explicit close/shutdown call (Rust has none to
/// skip — a `Database` binding being dropped at the end of its lexical
/// lifetime, with no flush step beyond what each committed operation
/// already durably wrote, IS the crash model: the process simply stops).
/// Every already-committed insert must still be there on reopen; nothing
/// partial from an interrupted operation should surface, because no
/// operation was left in flight.
#[test]
fn survives_crash_and_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crash.pinax");

    {
        let mut db = Database::create(&path, PageSize::DEFAULT).expect("create");
        for i in 0..50i64 {
            db.insert(i, &sample_row(i)).expect("insert before crash");
        }
        // No explicit close/shutdown: dropping `db` here models the crash.
        // Every insert above already committed (each `Database::insert`
        // call is its own auto-committed, fsynced transaction — see
        // `pager` module docs), so nothing here is left in flight.
    }

    let mut reopened = Database::open(&path).expect("reopen after simulated crash");
    for i in 0..50i64 {
        assert_eq!(
            reopened.get(i).expect("read after reopen"),
            Some(sample_row(i)),
            "row {i} must survive crash-and-reopen"
        );
    }
}

/// ROADMAP.md Phase 01: "page format checksums verified; corruption
/// detected". The negative-case fixture flips a byte in a page written to
/// disk and asserts the read path detects it — an intact-only test suite
/// would prove nothing about detection, only about the happy path.
#[test]
fn corruption_is_detected_via_checksum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.pinax");

    {
        let mut db = Database::create(&path, PageSize::DEFAULT).expect("create");
        db.insert(1, &sample_row(1)).expect("insert");
    }

    // Flip one byte inside the first data page's region on disk, bypassing
    // pinax entirely (models storage-level bit rot, not an API misuse).
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open db file directly");
    let offset = 8192; // meta region (2 * 4096) ends here; first data page begins here.
    let mut byte = [0u8; 1];
    file.read_exact_at(&mut byte, offset)
        .expect("read a byte of the data page");
    byte[0] ^= 0xFF;
    file.write_all_at(&byte, offset).expect("flip the byte");
    drop(file);

    let mut db = Database::open(&path).expect("meta pages are untouched, so open still succeeds");
    let err = db.get(1).expect_err("checksum must catch the flipped byte");
    assert!(
        matches!(
            err,
            PinaxError::Fatal {
                source: pinax::FatalError::Corruption { .. }
            }
        ),
        "expected a Corruption error, got {err:?}"
    );
}

/// ROADMAP.md Phase 01: "buffer pool handles databases larger than RAM".
///
/// "RAM" is modeled by the buffer pool's page capacity: a capacity of 8
/// pages caps resident memory at 8 * page_size regardless of how large the
/// on-disk B+tree grows, exactly the property a real buffer pool provides
/// against physical RAM. Inserting enough rows to produce a tree spanning
/// many times that capacity, then reading every row back correctly, proves
/// the pool's evict-and-reload path preserves data across pages the
/// working set could never hold all at once.
#[test]
fn buffer_pool_handles_a_database_larger_than_its_capacity() {
    const CAPACITY_PAGES: usize = 8;
    const ROW_COUNT: i64 = 4000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("larger-than-ram.pinax");

    {
        let mut db = Database::create_with_capacity(&path, PageSize::DEFAULT, CAPACITY_PAGES)
            .expect("create with a deliberately small buffer pool");
        for i in 0..ROW_COUNT {
            db.insert(i, &sample_row(i))
                .expect("insert under a small buffer pool");
        }
    }

    let capacity_bytes =
        u64::try_from(CAPACITY_PAGES).unwrap_or(0) * u64::from(PageSize::DEFAULT.bytes());
    let on_disk_bytes = std::fs::metadata(&path).expect("stat db file").len();
    assert!(
        on_disk_bytes > capacity_bytes * 4,
        "expected the on-disk database ({on_disk_bytes} bytes) to be several times \
         the buffer pool's capacity ({capacity_bytes} bytes) — otherwise this test \
         does not actually exercise eviction"
    );

    let mut db = Database::open_with_capacity(&path, CAPACITY_PAGES)
        .expect("reopen with the same small buffer pool");
    for i in 0..ROW_COUNT {
        assert_eq!(
            db.get(i).expect("read back under a small buffer pool"),
            Some(sample_row(i)),
            "row {i} must survive eviction-and-reload cycles"
        );
    }
}
