//! The copy-on-write B+tree (PLAN.md Decision 1): slotted leaf and
//! interior pages, overflow chains for cells over `max_local`, and
//! path-copying insert/delete that never mutates a page id already
//! reachable from the committed meta page.
//!
//! WHY path-copying rather than in-place mutation: every page on the
//! root-to-leaf path of a mutation gets a FRESH page id, and only the
//! final `BufferPool::commit` call makes the new root (and therefore the
//! whole new path) visible. This is what makes the crate's crash-safety
//! story (see `pager` module docs) work without a WAL — Phase 02's
//! deliverable, not Phase 01's.
//!
//! WHY no delete-time merge/rebalance: ROADMAP.md Phase 01's acceptance
//! criteria are CRUD correctness, crash safety, checksums, and buffer-pool
//! eviction — none require space-optimal trees under a delete-heavy
//! workload. A leaf/interior page is allowed to underflow after a delete —
//! `delete`'s interior-propagation step only ever repoints an existing
//! child pointer (`apply_result_to_interior`'s `Replaced` arm), it never
//! removes a separator key, so an ancestor's key count is monotonically
//! non-decreasing across its lifetime. `collapse_root_if_needed` handles
//! the one degenerate shape that IS still possible (an interior root with
//! zero separator keys) defensively rather than assuming it cannot occur;
//! nothing in Phase 01's delete path currently produces it. General
//! merge-on-underflow is real, deferred follow-up scope, not silently
//! dropped (#13).
//!
//! WHY split across four files: `RUST/file-too-long` (800-line limit) —
//! Phase 01 pushed the original single-file module past it. `layout`
//! (slotted-page primitives, leaf pages, interior pages), `overflow`
//! (overflow-chain read/write and the row spill/reassemble boundary), and
//! `mutate` (path-copying propagation) split along the section boundaries
//! the file already documented internally; this module keeps the
//! crate-facing public operations (`insert`/`get`/`update`/`delete`/
//! `scan`) and the page-layout constants every submodule shares.

mod layout;
mod mutate;
mod overflow;

use snafu::OptionExt as _;

use self::layout::{
    free_space, init_leaf, insert_cell_at, leaf_cell_at, leaf_cell_byte_len, leaf_search,
    num_cells, pointer_at, remove_cell_at,
};
use self::mutate::{
    NodeResult, apply_result_to_interior, collapse_root_if_needed, descend_path, finalize_root,
    interior_entries,
};
use self::overflow::{build_row_cell, reassemble};
use crate::buffer_pool::BufferPool;
use crate::codec::{read_i64, read_u8, read_vec};
use crate::error::{BufferBoundsSnafu, KeyAlreadyExistsSnafu, KeyNotFoundSnafu, PinaxError};
use crate::page::{PAGE_TYPE_INTERIOR, PAGE_TYPE_LEAF};
use crate::pager::Pager;
use crate::row::Row;

const LEAF_HEADER_LEN: usize = 5;
const INTERIOR_HEADER_LEN: usize = 9;
/// `key(8) + payload_len(4) + overflow_page(4)`; local row bytes follow.
const LEAF_CELL_FIXED_LEN: usize = 16;
/// `key(8) + child(4)`.
const INTERIOR_CELL_LEN: usize = 12;
const OVERFLOW_HEADER_LEN: usize = 5;
const POINTER_LEN: usize = 2;

/// Insert `row` under `key`. Returns the new root page id to commit.
///
/// # Errors
///
/// Returns [`crate::error::PermanentError::KeyAlreadyExists`] if `key` is
/// already present.
pub(crate) fn insert(pool: &mut BufferPool, key: i64, row: &Row) -> Result<u32, PinaxError> {
    let root = pool.root_page_id();

    if root == crate::page::EMPTY_TREE_ROOT {
        let cell = build_row_cell(pool, key, row)?;
        let mut buf = vec![0u8; pool.page_size().bytes_usize()];
        init_leaf(&mut buf)?;
        insert_cell_at(&mut buf, LEAF_HEADER_LEN, 0, &cell)?;
        let id = pool.allocate_page_id();
        pool.put_new(id, buf)?;
        pool.commit(id)?;
        return Ok(id);
    }

    let path = descend_path(pool, root, key)?;
    let leaf_id = *path.last().context(BufferBoundsSnafu {
        at: 0usize,
        len: 1usize,
        buf_len: 0usize,
    })?;
    let leaf_buf = pool.get(leaf_id)?;
    let insert_idx = match leaf_search(&leaf_buf, key)? {
        // WHY checked before `build_row_cell` below (which may allocate
        // overflow pages for a large row): a duplicate key must fail
        // before any work is done for a row that will not be stored — see
        // `build_row_cell`'s docs on why encode-before-check would still
        // be safe, just wasteful.
        //
        // WHY `?` rather than `return ....fail();`: `.fail()` builds the
        // LEAF error (`PermanentError`), one level below this function's
        // `PinaxError` — `?` performs the `From` conversion
        // `#[snafu(transparent)]` provides; a bare `return` would need
        // that type to already match exactly.
        Ok(_found) => KeyAlreadyExistsSnafu { key }.fail()?,
        Err(idx) => idx,
    };

    let cell = build_row_cell(pool, key, row)?;
    let mut result = leaf_insert_or_split(pool, &leaf_buf, insert_idx, &cell)?;
    let mut old_child_id = leaf_id;
    for &ancestor_id in path
        .get(..path.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        result = apply_result_to_interior(pool, ancestor_id, old_child_id, &result)?;
        old_child_id = ancestor_id;
    }

    let new_root = finalize_root(pool, result)?;
    pool.commit(new_root)?;
    Ok(new_root)
}

fn leaf_insert_or_split(
    pool: &mut BufferPool,
    leaf_buf: &[u8],
    insert_idx: usize,
    cell: &[u8],
) -> Result<NodeResult, PinaxError> {
    let needed = u16::try_from(cell.len() + POINTER_LEN).unwrap_or(u16::MAX);
    if free_space(leaf_buf, LEAF_HEADER_LEN)? >= needed {
        let mut buf = leaf_buf.to_vec();
        insert_cell_at(&mut buf, LEAF_HEADER_LEN, insert_idx, cell)?;
        let id = pool.allocate_page_id();
        pool.put_new(id, buf)?;
        return Ok(NodeResult::Replaced(id));
    }
    leaf_split_with_new_cell(pool, leaf_buf, insert_idx, cell)
}

fn leaf_split_with_new_cell(
    pool: &mut BufferPool,
    leaf_buf: &[u8],
    insert_idx: usize,
    new_cell: &[u8],
) -> Result<NodeResult, PinaxError> {
    let page_size = pool.page_size().bytes_usize();
    let max_local = pool.page_size().max_local();
    let n = usize::from(num_cells(leaf_buf)?);
    let mut all_cells: Vec<Vec<u8>> = Vec::with_capacity(n + 1);
    for i in 0..n {
        let offset = usize::from(pointer_at(leaf_buf, LEAF_HEADER_LEN, i)?);
        let len = usize::from(leaf_cell_byte_len(leaf_buf, i, max_local)?);
        all_cells.push(read_vec(leaf_buf, offset, len)?);
    }
    let clamped_idx = insert_idx.min(all_cells.len());
    all_cells.insert(clamped_idx, new_cell.to_vec());

    let mid = all_cells.len() / 2;
    let (left_half, right_half) = all_cells.split_at(mid);
    let mut left_buf = vec![0u8; page_size];
    init_leaf(&mut left_buf)?;
    for (i, cell) in left_half.iter().enumerate() {
        insert_cell_at(&mut left_buf, LEAF_HEADER_LEN, i, cell)?;
    }
    let mut right_buf = vec![0u8; page_size];
    init_leaf(&mut right_buf)?;
    for (i, cell) in right_half.iter().enumerate() {
        insert_cell_at(&mut right_buf, LEAF_HEADER_LEN, i, cell)?;
    }
    let separator_key = read_i64(
        right_half.first().context(BufferBoundsSnafu {
            at: 0usize,
            len: 1usize,
            buf_len: 0usize,
        })?,
        0,
    )?;

    let left_id = pool.allocate_page_id();
    pool.put_new(left_id, left_buf)?;
    let right_id = pool.allocate_page_id();
    pool.put_new(right_id, right_buf)?;
    Ok(NodeResult::Split {
        left: left_id,
        right: right_id,
        separator_key,
    })
}

/// Look up `key`. Returns `None` if absent.
pub(crate) fn get(pool: &mut BufferPool, key: i64) -> Result<Option<Row>, PinaxError> {
    let root = pool.root_page_id();
    if root == crate::page::EMPTY_TREE_ROOT {
        return Ok(None);
    }
    let max_local = pool.page_size().max_local();
    let mut current = root;
    loop {
        let buf = pool.get(current)?;
        let page_type = read_u8(&buf, 0)?;
        if page_type == PAGE_TYPE_LEAF {
            return match leaf_search(&buf, key)? {
                Ok(idx) => {
                    let cell = leaf_cell_at(&buf, idx, max_local)?;
                    let full = reassemble(pool, &cell)?;
                    Ok(Some(Row::decode(&full)?))
                }
                Err(_) => Ok(None),
            };
        }
        Pager::expect_page_type(current, &buf, PAGE_TYPE_INTERIOR, "interior")?;
        current = layout::interior_find_child_for_key(&buf, key)?;
    }
}

/// Replace the row stored at `key`.
///
/// # Errors
///
/// Returns [`crate::error::PermanentError::KeyNotFound`] if `key` is
/// absent.
pub(crate) fn update(pool: &mut BufferPool, key: i64, row: &Row) -> Result<u32, PinaxError> {
    let root = pool.root_page_id();
    if root == crate::page::EMPTY_TREE_ROOT {
        KeyNotFoundSnafu { key }.fail()?;
    }
    let path = descend_path(pool, root, key)?;
    let leaf_id = *path.last().context(BufferBoundsSnafu {
        at: 0usize,
        len: 1usize,
        buf_len: 0usize,
    })?;
    let leaf_buf = pool.get(leaf_id)?;
    let idx = match leaf_search(&leaf_buf, key)? {
        Ok(idx) => idx,
        Err(_) => KeyNotFoundSnafu { key }.fail()?,
    };

    let new_cell = build_row_cell(pool, key, row)?;

    let mut buf = leaf_buf.clone();
    remove_cell_at(&mut buf, LEAF_HEADER_LEN, idx)?;
    let mut result = leaf_insert_or_split(pool, &buf, idx, &new_cell)?;
    let mut old_child_id = leaf_id;
    for &ancestor_id in path
        .get(..path.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        result = apply_result_to_interior(pool, ancestor_id, old_child_id, &result)?;
        old_child_id = ancestor_id;
    }
    let new_root = finalize_root(pool, result)?;
    pool.commit(new_root)?;
    Ok(new_root)
}

/// Delete the row stored at `key`, returning it.
///
/// # Errors
///
/// Returns [`crate::error::PermanentError::KeyNotFound`] if `key` is
/// absent.
pub(crate) fn delete(pool: &mut BufferPool, key: i64) -> Result<(u32, Row), PinaxError> {
    let root = pool.root_page_id();
    if root == crate::page::EMPTY_TREE_ROOT {
        KeyNotFoundSnafu { key }.fail()?;
    }
    let max_local = pool.page_size().max_local();
    let path = descend_path(pool, root, key)?;
    let leaf_id = *path.last().context(BufferBoundsSnafu {
        at: 0usize,
        len: 1usize,
        buf_len: 0usize,
    })?;
    let leaf_buf = pool.get(leaf_id)?;
    let idx = match leaf_search(&leaf_buf, key)? {
        Ok(idx) => idx,
        Err(_) => KeyNotFoundSnafu { key }.fail()?,
    };
    let removed_cell = leaf_cell_at(&leaf_buf, idx, max_local)?;
    let removed_full = reassemble(pool, &removed_cell)?;
    let removed_row = Row::decode(&removed_full)?;

    let mut buf = leaf_buf.clone();
    remove_cell_at(&mut buf, LEAF_HEADER_LEN, idx)?;
    let new_leaf_id = pool.allocate_page_id();
    pool.put_new(new_leaf_id, buf)?;
    let mut result = NodeResult::Replaced(new_leaf_id);
    let mut old_child_id = leaf_id;
    for &ancestor_id in path
        .get(..path.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .rev()
    {
        result = apply_result_to_interior(pool, ancestor_id, old_child_id, &result)?;
        old_child_id = ancestor_id;
    }
    let mut new_root = finalize_root(pool, result)?;
    new_root = collapse_root_if_needed(pool, new_root)?;
    pool.commit(new_root)?;
    Ok((new_root, removed_row))
}

/// In-order traversal of every `(key, row)` pair. Recursive over the
/// tree's own height (bounded by page fan-out), not sibling-linked — see
/// module docs on why Phase 01 has no leaf sibling pointers.
pub(crate) fn scan(pool: &mut BufferPool) -> Result<Vec<(i64, Row)>, PinaxError> {
    let root = pool.root_page_id();
    let mut out = Vec::new();
    if root != crate::page::EMPTY_TREE_ROOT {
        scan_node(pool, root, &mut out)?;
    }
    Ok(out)
}

fn scan_node(pool: &mut BufferPool, id: u32, out: &mut Vec<(i64, Row)>) -> Result<(), PinaxError> {
    let buf = pool.get(id)?;
    let page_type = read_u8(&buf, 0)?;
    if page_type == PAGE_TYPE_LEAF {
        let max_local = pool.page_size().max_local();
        let n = usize::from(num_cells(&buf)?);
        for i in 0..n {
            let cell = leaf_cell_at(&buf, i, max_local)?;
            let full = reassemble(pool, &cell)?;
            let row = Row::decode(&full)?;
            out.push((cell.key, row));
        }
        return Ok(());
    }
    Pager::expect_page_type(id, &buf, PAGE_TYPE_INTERIOR, "interior")?;
    let (_, children) = interior_entries(&buf)?;
    for child in children {
        scan_node(pool, child, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use lexis::Value;

    use super::*;
    use crate::page::PageSize;
    use crate::pager::Pager;

    fn pool(dir: &tempfile::TempDir, capacity: usize) -> BufferPool {
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        BufferPool::new(pager, capacity).expect("valid capacity")
    }

    fn row(n: i64) -> Row {
        Row::new(vec![Value::Integer(n), Value::Text(format!("row-{n}"))])
    }

    #[test]
    fn insert_then_get_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        insert(&mut pool, 1, &row(1)).expect("insert");
        let got = get(&mut pool, 1).expect("get").expect("present");
        assert_eq!(got, row(1));
    }

    #[test]
    fn get_missing_key_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        assert_eq!(get(&mut pool, 42).expect("get"), None);
    }

    #[test]
    fn insert_duplicate_key_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        insert(&mut pool, 1, &row(1)).expect("first insert");
        let err = insert(&mut pool, 1, &row(2)).expect_err("duplicate key");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: crate::error::PermanentError::KeyAlreadyExists { .. }
            }
        ));
    }

    #[test]
    fn update_replaces_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        insert(&mut pool, 1, &row(1)).expect("insert");
        update(&mut pool, 1, &row(99)).expect("update");
        assert_eq!(get(&mut pool, 1).expect("get").expect("present"), row(99));
    }

    #[test]
    fn update_missing_key_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        let err = update(&mut pool, 1, &row(1)).expect_err("no such key");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: crate::error::PermanentError::KeyNotFound { .. }
            }
        ));
    }

    #[test]
    fn delete_removes_and_returns_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        insert(&mut pool, 1, &row(1)).expect("insert");
        let (_, removed) = delete(&mut pool, 1).expect("delete");
        assert_eq!(removed, row(1));
        assert_eq!(get(&mut pool, 1).expect("get"), None);
    }

    #[test]
    fn delete_missing_key_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        let err = delete(&mut pool, 1).expect_err("no such key");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: crate::error::PermanentError::KeyNotFound { .. }
            }
        ));
    }

    #[test]
    fn many_inserts_force_splits_and_all_keys_remain_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 1024);
        for i in 0..500i64 {
            insert(&mut pool, i, &row(i)).expect("insert");
        }
        for i in 0..500i64 {
            assert_eq!(get(&mut pool, i).expect("get").expect("present"), row(i));
        }
    }

    #[test]
    fn insert_out_of_order_keys_stay_sorted_and_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 1024);
        let keys: Vec<i64> = vec![50, 10, 90, 30, 70, 20, 80, 40, 60, 0];
        for &k in &keys {
            insert(&mut pool, k, &row(k)).expect("insert");
        }
        for &k in &keys {
            assert_eq!(get(&mut pool, k).expect("get").expect("present"), row(k));
        }
        let scanned = scan(&mut pool).expect("scan");
        let scanned_keys: Vec<i64> = scanned.iter().map(|(k, _)| *k).collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort_unstable();
        assert_eq!(scanned_keys, sorted_keys);
    }

    #[test]
    fn negative_and_extreme_keys_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        for &k in &[i64::MIN, -1, 0, 1, i64::MAX] {
            insert(&mut pool, k, &row(k)).expect("insert");
        }
        for &k in &[i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(get(&mut pool, k).expect("get").expect("present"), row(k));
        }
    }

    #[test]
    fn large_value_spills_to_overflow_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        let big_text = "x".repeat(20_000);
        let big_row = Row::new(vec![Value::Text(big_text.clone())]);
        insert(&mut pool, 1, &big_row).expect("insert with overflow");
        let got = get(&mut pool, 1).expect("get").expect("present");
        assert_eq!(got.values(), &[Value::Text(big_text)]);
    }

    #[test]
    fn delete_then_reinsert_same_key_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 64);
        insert(&mut pool, 1, &row(1)).expect("insert");
        delete(&mut pool, 1).expect("delete");
        insert(&mut pool, 1, &row(2)).expect("reinsert");
        assert_eq!(get(&mut pool, 1).expect("get").expect("present"), row(2));
    }

    #[test]
    fn delete_most_of_a_multi_level_tree_leaves_remaining_keys_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 1024);
        for i in 0..300i64 {
            insert(&mut pool, i, &row(i)).expect("insert");
        }
        for i in 0..250i64 {
            delete(&mut pool, i).expect("delete");
        }
        for i in 0..250i64 {
            assert_eq!(get(&mut pool, i).expect("get"), None);
        }
        for i in 250..300i64 {
            assert_eq!(get(&mut pool, i).expect("get").expect("present"), row(i));
        }
    }

    #[test]
    fn insert_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("db.pinax");
        {
            let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
            let mut pool = BufferPool::new(pager, 64).expect("valid capacity");
            for i in 0..20i64 {
                insert(&mut pool, i, &row(i)).expect("insert");
            }
        }
        let pager = Pager::open(&path).expect("reopen");
        let mut pool = BufferPool::new(pager, 64).expect("valid capacity");
        for i in 0..20i64 {
            assert_eq!(get(&mut pool, i).expect("get").expect("present"), row(i));
        }
    }
}
