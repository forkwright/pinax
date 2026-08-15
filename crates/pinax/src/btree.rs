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
//! merge-on-underflow is tracked as deliberate follow-up scope, not
//! silently dropped.

use snafu::OptionExt as _;

use crate::buffer_pool::BufferPool;
use crate::codec::{
    read_i64, read_u8, read_u16, read_u32, read_vec, write_u8, write_u16, write_u32,
};
use crate::error::{BufferBoundsSnafu, KeyAlreadyExistsSnafu, KeyNotFoundSnafu, PinaxError};
use crate::page::{PAGE_TYPE_INTERIOR, PAGE_TYPE_LEAF, PAGE_TYPE_OVERFLOW};
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

// ---------------------------------------------------------------------
// Generic slotted-page primitives, shared by leaf and interior pages.
// Layout: [header][pointer array, ascending][... free ...][cell content
// area, descending toward the checksum trailer].
// ---------------------------------------------------------------------

fn init_slotted(buf: &mut [u8], page_type: u8) -> Result<(), PinaxError> {
    let usable = u16::try_from(buf.len()).unwrap_or(u16::MAX) - crate::page::checksum_len_u16();
    write_u8(buf, 0, page_type)?;
    write_u16(buf, 1, 0)?;
    write_u16(buf, 3, usable)?;
    Ok(())
}

fn num_cells(buf: &[u8]) -> Result<u16, PinaxError> {
    read_u16(buf, 1)
}

fn set_num_cells(buf: &mut [u8], n: u16) -> Result<(), PinaxError> {
    write_u16(buf, 1, n)
}

fn content_start(buf: &[u8]) -> Result<u16, PinaxError> {
    read_u16(buf, 3)
}

fn set_content_start(buf: &mut [u8], v: u16) -> Result<(), PinaxError> {
    write_u16(buf, 3, v)
}

fn pointer_at(buf: &[u8], header_len: usize, index: usize) -> Result<u16, PinaxError> {
    read_u16(buf, header_len + index * POINTER_LEN)
}

fn set_pointer_at(
    buf: &mut [u8],
    header_len: usize,
    index: usize,
    offset: u16,
) -> Result<(), PinaxError> {
    write_u16(buf, header_len + index * POINTER_LEN, offset)
}

fn free_space(buf: &[u8], header_len: usize) -> Result<u16, PinaxError> {
    let n = num_cells(buf)?;
    let cs = content_start(buf)?;
    let header_len_u16 = u16::try_from(header_len).unwrap_or(u16::MAX);
    let used_by_pointers = header_len_u16 + n * u16::try_from(POINTER_LEN).unwrap_or(2);
    Ok(cs.saturating_sub(used_by_pointers))
}

/// Insert `cell_bytes` as a new cell at pointer-array `index`, shifting
/// later pointers right. Caller must have already verified `free_space`
/// covers `cell_bytes.len() + POINTER_LEN`.
fn insert_cell_at(
    buf: &mut [u8],
    header_len: usize,
    index: usize,
    cell_bytes: &[u8],
) -> Result<(), PinaxError> {
    let n = usize::from(num_cells(buf)?);
    let cs = content_start(buf)?;
    let cell_len = u16::try_from(cell_bytes.len()).unwrap_or(u16::MAX);
    let new_cs = cs - cell_len;
    crate::codec::write_bytes(buf, usize::from(new_cs), cell_bytes)?;
    for i in (index..n).rev() {
        let p = pointer_at(buf, header_len, i)?;
        set_pointer_at(buf, header_len, i + 1, p)?;
    }
    set_pointer_at(buf, header_len, index, new_cs)?;
    set_num_cells(buf, u16::try_from(n + 1).unwrap_or(u16::MAX))?;
    set_content_start(buf, new_cs)?;
    Ok(())
}

/// Remove the cell at pointer-array `index`. Returns its former content
/// offset so the caller can read it BEFORE calling this (removal never
/// reclaims content-area space — see module docs on deferred compaction).
fn remove_cell_at(buf: &mut [u8], header_len: usize, index: usize) -> Result<u16, PinaxError> {
    let n = usize::from(num_cells(buf)?);
    let offset = pointer_at(buf, header_len, index)?;
    for i in index..n.saturating_sub(1) {
        let p = pointer_at(buf, header_len, i + 1)?;
        set_pointer_at(buf, header_len, i, p)?;
    }
    set_num_cells(buf, u16::try_from(n.saturating_sub(1)).unwrap_or(0))?;
    Ok(offset)
}

// ---------------------------------------------------------------------
// Leaf pages.
// ---------------------------------------------------------------------

fn init_leaf(buf: &mut [u8]) -> Result<(), PinaxError> {
    init_slotted(buf, PAGE_TYPE_LEAF)
}

fn leaf_key_at(buf: &[u8], index: usize) -> Result<i64, PinaxError> {
    let offset = pointer_at(buf, LEAF_HEADER_LEN, index)?;
    read_i64(buf, usize::from(offset))
}

struct LeafCell {
    key: i64,
    payload_len: u32,
    overflow_first: u32,
    local: Vec<u8>,
}

fn leaf_local_len(payload_len: u32, max_local: u32) -> u32 {
    payload_len.min(max_local)
}

fn leaf_cell_at(buf: &[u8], index: usize, max_local: u32) -> Result<LeafCell, PinaxError> {
    let offset = usize::from(pointer_at(buf, LEAF_HEADER_LEN, index)?);
    let key = read_i64(buf, offset)?;
    let payload_len = read_u32(buf, offset + 8)?;
    let overflow_first = read_u32(buf, offset + 12)?;
    let local_len = usize::try_from(leaf_local_len(payload_len, max_local)).unwrap_or(0);
    let local = read_vec(buf, offset + LEAF_CELL_FIXED_LEN, local_len)?;
    Ok(LeafCell {
        key,
        payload_len,
        overflow_first,
        local,
    })
}

fn leaf_cell_byte_len(buf: &[u8], index: usize, max_local: u32) -> Result<u16, PinaxError> {
    let offset = usize::from(pointer_at(buf, LEAF_HEADER_LEN, index)?);
    let payload_len = read_u32(buf, offset + 8)?;
    let local_len = leaf_local_len(payload_len, max_local);
    Ok(u16::try_from(LEAF_CELL_FIXED_LEN).unwrap_or(16) + u16::try_from(local_len).unwrap_or(0))
}

fn build_leaf_cell(key: i64, payload_len: u32, overflow_first: u32, local: &[u8]) -> Vec<u8> {
    let mut cell = Vec::with_capacity(LEAF_CELL_FIXED_LEN + local.len());
    cell.extend_from_slice(&key.to_be_bytes());
    cell.extend_from_slice(&payload_len.to_be_bytes());
    cell.extend_from_slice(&overflow_first.to_be_bytes());
    cell.extend_from_slice(local);
    cell
}

/// Binary search a leaf's sorted keys for `key`. `Ok(i)` if present at
/// index `i`; `Err(i)` for the sorted insertion point otherwise.
fn leaf_search(buf: &[u8], key: i64) -> Result<Result<usize, usize>, PinaxError> {
    let n = usize::from(num_cells(buf)?);
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mid_key = leaf_key_at(buf, mid)?;
        match mid_key.cmp(&key) {
            std::cmp::Ordering::Equal => return Ok(Ok(mid)),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Ok(Err(lo))
}

// ---------------------------------------------------------------------
// Interior pages.
// ---------------------------------------------------------------------

fn init_interior(buf: &mut [u8], rightmost_child: u32) -> Result<(), PinaxError> {
    init_slotted(buf, PAGE_TYPE_INTERIOR)?;
    write_u32(buf, 5, rightmost_child)
}

fn interior_rightmost(buf: &[u8]) -> Result<u32, PinaxError> {
    read_u32(buf, 5)
}

fn interior_set_rightmost(buf: &mut [u8], child: u32) -> Result<(), PinaxError> {
    write_u32(buf, 5, child)
}

fn interior_key_at(buf: &[u8], index: usize) -> Result<i64, PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    read_i64(buf, usize::from(offset))
}

fn interior_child_at(buf: &[u8], index: usize) -> Result<u32, PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    read_u32(buf, usize::from(offset) + 8)
}

fn interior_set_child_at(buf: &mut [u8], index: usize, child: u32) -> Result<(), PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    write_u32(buf, usize::from(offset) + 8, child)
}

fn build_interior_cell(key: i64, child: u32) -> Vec<u8> {
    let mut cell = Vec::with_capacity(INTERIOR_CELL_LEN);
    cell.extend_from_slice(&key.to_be_bytes());
    cell.extend_from_slice(&child.to_be_bytes());
    cell
}

/// Which child of an interior page an id is referenced from.
enum ChildSlot {
    Cell(usize),
    Rightmost,
}

fn interior_find_child_slot(buf: &[u8], child_id: u32) -> Result<ChildSlot, PinaxError> {
    let n = usize::from(num_cells(buf)?);
    for i in 0..n {
        if interior_child_at(buf, i)? == child_id {
            return Ok(ChildSlot::Cell(i));
        }
    }
    Ok(ChildSlot::Rightmost)
}

/// Route `key` to the child that should hold it: the first cell whose key
/// exceeds `key`, or the rightmost child if `key` is at least every
/// separator.
fn interior_find_child_for_key(buf: &[u8], key: i64) -> Result<u32, PinaxError> {
    let n = usize::from(num_cells(buf)?);
    for i in 0..n {
        if key < interior_key_at(buf, i)? {
            return interior_child_at(buf, i);
        }
    }
    interior_rightmost(buf)
}

// ---------------------------------------------------------------------
// Overflow chains.
// ---------------------------------------------------------------------

fn overflow_chunk_cap(page_size_bytes: u32) -> usize {
    let usable = page_size_bytes - crate::page::checksum_len_u32();
    usize::try_from(usable)
        .unwrap_or(0)
        .saturating_sub(OVERFLOW_HEADER_LEN)
}

fn write_overflow_chain(pool: &mut BufferPool, tail: &[u8]) -> Result<u32, PinaxError> {
    if tail.is_empty() {
        return Ok(0);
    }
    let chunk_cap = overflow_chunk_cap(pool.page_size().bytes());
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut end = tail.len();
    while end > 0 {
        let start = end.saturating_sub(chunk_cap);
        let chunk = tail.get(start..end).context(BufferBoundsSnafu {
            at: start,
            len: end - start,
            buf_len: tail.len(),
        })?;
        chunks.push(chunk);
        end = start;
    }

    let mut next_id = 0u32;
    for chunk in chunks {
        let mut buf = vec![0u8; pool.page_size().bytes_usize()];
        write_u8(&mut buf, 0, PAGE_TYPE_OVERFLOW)?;
        write_u32(&mut buf, 1, next_id)?;
        crate::codec::write_bytes(&mut buf, OVERFLOW_HEADER_LEN, chunk)?;
        let id = pool.allocate_page_id();
        pool.put_new(id, buf)?;
        next_id = id;
    }
    Ok(next_id)
}

fn read_overflow_chain(
    pool: &mut BufferPool,
    first_id: u32,
    total_len: usize,
) -> Result<Vec<u8>, PinaxError> {
    let mut out = Vec::with_capacity(total_len.min(1 << 20));
    let mut current = first_id;
    while current != 0 && out.len() < total_len {
        let buf = pool.get(current)?;
        Pager::expect_page_type(current, &buf, PAGE_TYPE_OVERFLOW, "overflow")?;
        let next = read_u32(&buf, 1)?;
        let remaining_needed = total_len - out.len();
        let chunk_cap = overflow_chunk_cap(pool.page_size().bytes());
        let take = remaining_needed.min(chunk_cap);
        let mut chunk = read_vec(&buf, OVERFLOW_HEADER_LEN, take)?;
        out.append(&mut chunk);
        current = next;
    }
    Ok(out)
}

/// Split `encoded` into (local bytes kept in the leaf cell, first overflow
/// page id or 0) per Decision 2's `max_local` threshold.
fn spill_if_needed(pool: &mut BufferPool, encoded: &[u8]) -> Result<(Vec<u8>, u32), PinaxError> {
    let max_local = usize::try_from(pool.page_size().max_local()).unwrap_or(0);
    if encoded.len() <= max_local {
        return Ok((encoded.to_vec(), 0));
    }
    let local = encoded.get(..max_local).context(BufferBoundsSnafu {
        at: 0,
        len: max_local,
        buf_len: encoded.len(),
    })?;
    let tail = encoded.get(max_local..).context(BufferBoundsSnafu {
        at: max_local,
        len: encoded.len() - max_local,
        buf_len: encoded.len(),
    })?;
    let overflow_first = write_overflow_chain(pool, tail)?;
    Ok((local.to_vec(), overflow_first))
}

/// Encode `row`, spill it past `max_local` if needed (possibly allocating
/// overflow pages — see [`spill_if_needed`]), and build the resulting leaf
/// cell bytes.
///
/// WHY callers check `leaf_search` for a duplicate/missing key BEFORE
/// calling this rather than after: encoding and spilling a large row is
/// real, possibly page-allocating work. Doing it before the key check
/// would still be crash-safe (an aborted `insert`/`update` just leaves a
/// few page ids allocated-but-unreferenced — see `pager` module docs on
/// why that is harmless), so this ordering is an efficiency choice, not a
/// correctness one.
fn build_row_cell(pool: &mut BufferPool, key: i64, row: &Row) -> Result<Vec<u8>, PinaxError> {
    let encoded = row.encode(key)?;
    let (local, overflow_first) = spill_if_needed(pool, &encoded)?;
    let payload_len = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
    Ok(build_leaf_cell(key, payload_len, overflow_first, &local))
}

/// Reassemble a leaf cell's full encoded payload (local bytes plus any
/// overflow chain).
fn reassemble(pool: &mut BufferPool, cell: &LeafCell) -> Result<Vec<u8>, PinaxError> {
    if cell.overflow_first == 0 {
        return Ok(cell.local.clone());
    }
    let max_local = pool.page_size().max_local();
    let tail_len = usize::try_from(cell.payload_len.saturating_sub(max_local)).unwrap_or(0);
    let mut full = cell.local.clone();
    let mut tail = read_overflow_chain(pool, cell.overflow_first, tail_len)?;
    full.append(&mut tail);
    Ok(full)
}

// ---------------------------------------------------------------------
// Path-copying mutation result and propagation.
// ---------------------------------------------------------------------

enum NodeResult {
    Replaced(u32),
    Split {
        left: u32,
        right: u32,
        separator_key: i64,
    },
}

fn descend_path(pool: &mut BufferPool, root: u32, key: i64) -> Result<Vec<u32>, PinaxError> {
    let mut path = vec![root];
    let mut current = root;
    loop {
        let buf = pool.get(current)?;
        let page_type = read_u8(&buf, 0)?;
        if page_type == PAGE_TYPE_LEAF {
            return Ok(path);
        }
        Pager::expect_page_type(current, &buf, PAGE_TYPE_INTERIOR, "interior")?;
        current = interior_find_child_for_key(&buf, key)?;
        path.push(current);
    }
}

/// Collect an interior page's keys and children as growable vectors —
/// `children.len() == keys.len() + 1`, with the last entry the rightmost
/// child — so insert-then-split logic can operate uniformly.
fn interior_entries(buf: &[u8]) -> Result<(Vec<i64>, Vec<u32>), PinaxError> {
    let n = usize::from(num_cells(buf)?);
    let mut keys = Vec::with_capacity(n);
    let mut children = Vec::with_capacity(n + 1);
    for i in 0..n {
        keys.push(interior_key_at(buf, i)?);
        children.push(interior_child_at(buf, i)?);
    }
    children.push(interior_rightmost(buf)?);
    Ok((keys, children))
}

fn build_interior_page(
    page_size: usize,
    keys: &[i64],
    children: &[u32],
) -> Result<Vec<u8>, PinaxError> {
    let mut buf = vec![0u8; page_size];
    let rightmost = *children.last().unwrap_or(&0);
    init_interior(&mut buf, rightmost)?;
    for (i, &key) in keys.iter().enumerate() {
        let child = *children.get(i).unwrap_or(&0);
        let cell = build_interior_cell(key, child);
        insert_cell_at(&mut buf, INTERIOR_HEADER_LEN, i, &cell)?;
    }
    Ok(buf)
}

/// Insert `(separator_key, left_child)` into `keys`/`children` at the
/// position `old_child_id` used to occupy, replacing that position's
/// child with `right_child` (the standard B+tree "a child split into two"
/// update — see module docs).
fn splice_split_into_entries(
    keys: &mut Vec<i64>,
    children: &mut Vec<u32>,
    old_child_id: u32,
    separator_key: i64,
    left_child: u32,
    right_child: u32,
) {
    let position = children
        .iter()
        .position(|&c| c == old_child_id)
        .unwrap_or(children.len().saturating_sub(1));
    keys.insert(position, separator_key);
    children.insert(position, left_child);
    if let Some(slot) = children.get_mut(position + 1) {
        *slot = right_child;
    }
}

fn apply_result_to_interior(
    pool: &mut BufferPool,
    ancestor_id: u32,
    old_child_id: u32,
    result: &NodeResult,
) -> Result<NodeResult, PinaxError> {
    let page_size = pool.page_size().bytes_usize();
    match result {
        NodeResult::Replaced(new_child) => {
            let mut buf = pool.get(ancestor_id)?;
            match interior_find_child_slot(&buf, old_child_id)? {
                ChildSlot::Cell(idx) => interior_set_child_at(&mut buf, idx, *new_child)?,
                ChildSlot::Rightmost => interior_set_rightmost(&mut buf, *new_child)?,
            }
            let new_id = pool.allocate_page_id();
            pool.put_new(new_id, buf)?;
            Ok(NodeResult::Replaced(new_id))
        }
        NodeResult::Split {
            left,
            right,
            separator_key,
        } => {
            let buf = pool.get(ancestor_id)?;
            let (mut keys, mut children) = interior_entries(&buf)?;
            splice_split_into_entries(
                &mut keys,
                &mut children,
                old_child_id,
                *separator_key,
                *left,
                *right,
            );
            if keys.len() <= max_interior_entries(page_size) {
                let rebuilt = build_interior_page(page_size, &keys, &children)?;
                let new_id = pool.allocate_page_id();
                pool.put_new(new_id, rebuilt)?;
                Ok(NodeResult::Replaced(new_id))
            } else {
                split_interior_entries(pool, page_size, &keys, &children)
            }
        }
    }
}

/// The exact number of fixed-size separator-key cells that fit on one
/// otherwise-empty interior page: `(usable_space - header) / (cell +
/// pointer)`, matching how `free_space` accounts for the same page.
fn max_interior_entries(page_size: usize) -> usize {
    let usable = page_size.saturating_sub(8);
    let per_cell = INTERIOR_CELL_LEN + POINTER_LEN;
    usable.saturating_sub(INTERIOR_HEADER_LEN) / per_cell.max(1)
}

fn split_interior_entries(
    pool: &mut BufferPool,
    page_size: usize,
    keys: &[i64],
    children: &[u32],
) -> Result<NodeResult, PinaxError> {
    let mid = keys.len() / 2;
    let promoted = *keys.get(mid).context(BufferBoundsSnafu {
        at: mid,
        len: 1,
        buf_len: keys.len(),
    })?;

    let left_keys = keys.get(..mid).unwrap_or(&[]);
    let left_children = children.get(..=mid).unwrap_or(&[]);
    let right_keys = keys.get(mid + 1..).unwrap_or(&[]);
    let right_children = children.get(mid + 1..).unwrap_or(&[]);

    let left_buf = build_interior_page(page_size, left_keys, left_children)?;
    let right_buf = build_interior_page(page_size, right_keys, right_children)?;
    let left_id = pool.allocate_page_id();
    pool.put_new(left_id, left_buf)?;
    let right_id = pool.allocate_page_id();
    pool.put_new(right_id, right_buf)?;
    Ok(NodeResult::Split {
        left: left_id,
        right: right_id,
        separator_key: promoted,
    })
}

fn finalize_root(pool: &mut BufferPool, result: NodeResult) -> Result<u32, PinaxError> {
    match result {
        NodeResult::Replaced(id) => Ok(id),
        NodeResult::Split {
            left,
            right,
            separator_key,
        } => {
            let mut buf = vec![0u8; pool.page_size().bytes_usize()];
            init_interior(&mut buf, right)?;
            let cell = build_interior_cell(separator_key, left);
            insert_cell_at(&mut buf, INTERIOR_HEADER_LEN, 0, &cell)?;
            let id = pool.allocate_page_id();
            pool.put_new(id, buf)?;
            Ok(id)
        }
    }
}

/// Collapse an interior root with zero separator keys to its sole
/// (rightmost) child, defensively — see module docs on why nothing in
/// Phase 01's current delete path actually produces this shape yet.
fn collapse_root_if_needed(pool: &mut BufferPool, root: u32) -> Result<u32, PinaxError> {
    let buf = pool.get(root)?;
    if read_u8(&buf, 0)? != PAGE_TYPE_INTERIOR {
        return Ok(root);
    }
    if num_cells(&buf)? == 0 {
        return Ok(interior_rightmost(&buf)?);
    }
    Ok(root)
}

// ---------------------------------------------------------------------
// Public B+tree operations.
// ---------------------------------------------------------------------

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
        at: 0,
        len: 1,
        buf_len: 0,
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
            at: 0,
            len: 1,
            buf_len: 0,
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
        current = interior_find_child_for_key(&buf, key)?;
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
        at: 0,
        len: 1,
        buf_len: 0,
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
        at: 0,
        len: 1,
        buf_len: 0,
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
    use super::*;
    use crate::page::PageSize;
    use crate::pager::Pager;
    use lexis::Value;

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

    #[test]
    fn collapse_root_if_needed_collapses_a_zero_key_interior_root() {
        // WHY built directly rather than reached through public
        // insert/delete: Phase 01's delete path never produces a
        // zero-separator-key interior root (see module docs on why an
        // ancestor's key count is monotonically non-decreasing) — this
        // exercises the defensive branch on its own.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 16);

        let leaf_id = pool.allocate_page_id();
        let mut leaf_buf = vec![0u8; pool.page_size().bytes_usize()];
        init_leaf(&mut leaf_buf).expect("init leaf");
        pool.put_new(leaf_id, leaf_buf).expect("put leaf");

        let mut interior_buf = vec![0u8; pool.page_size().bytes_usize()];
        init_interior(&mut interior_buf, leaf_id).expect("init interior with zero keys");
        let interior_id = pool.allocate_page_id();
        pool.put_new(interior_id, interior_buf)
            .expect("put interior");

        let collapsed = collapse_root_if_needed(&mut pool, interior_id).expect("collapse");
        assert_eq!(collapsed, leaf_id);
    }

    #[test]
    fn collapse_root_if_needed_leaves_a_leaf_root_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pool = pool(&dir, 16);
        let leaf_id = pool.allocate_page_id();
        let mut leaf_buf = vec![0u8; pool.page_size().bytes_usize()];
        init_leaf(&mut leaf_buf).expect("init leaf");
        pool.put_new(leaf_id, leaf_buf).expect("put leaf");

        let result = collapse_root_if_needed(&mut pool, leaf_id).expect("no-op on a leaf root");
        assert_eq!(result, leaf_id);
    }
}
