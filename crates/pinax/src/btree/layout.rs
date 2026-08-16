//! Slotted-page primitives shared by leaf and interior pages, plus the
//! leaf-cell and interior-cell layouts built on top of them.
//!
//! Layout: `[header][pointer array, ascending][... free ...][cell content
//! area, descending toward the checksum trailer]`.
//!
//! WHY this is its own file: split out of `btree.rs` once Phase 01 pushed
//! that file past `RUST/file-too-long`'s 800-line limit, along the section
//! boundary the file already documented internally. See `super`'s module
//! doc for the full split rationale.

use super::{
    INTERIOR_CELL_LEN, INTERIOR_HEADER_LEN, LEAF_CELL_FIXED_LEN, LEAF_HEADER_LEN, POINTER_LEN,
};
use crate::codec::{read_i64, read_u16, read_u32, read_vec, write_u8, write_u16, write_u32};
use crate::error::PinaxError;
use crate::page::{PAGE_TYPE_INTERIOR, PAGE_TYPE_LEAF};

fn init_slotted(buf: &mut [u8], page_type: u8) -> Result<(), PinaxError> {
    let usable = u16::try_from(buf.len()).unwrap_or(u16::MAX) - crate::page::checksum_len_u16();
    write_u8(buf, 0, page_type)?;
    write_u16(buf, 1, 0)?;
    write_u16(buf, 3, usable)?;
    Ok(())
}

pub(super) fn num_cells(buf: &[u8]) -> Result<u16, PinaxError> {
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

pub(super) fn pointer_at(buf: &[u8], header_len: usize, index: usize) -> Result<u16, PinaxError> {
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

pub(super) fn free_space(buf: &[u8], header_len: usize) -> Result<u16, PinaxError> {
    let n = num_cells(buf)?;
    let cs = content_start(buf)?;
    let header_len_u16 = u16::try_from(header_len).unwrap_or(u16::MAX);
    let used_by_pointers = header_len_u16 + n * u16::try_from(POINTER_LEN).unwrap_or(2);
    Ok(cs.saturating_sub(used_by_pointers))
}

/// Insert `cell_bytes` as a new cell at pointer-array `index`, shifting
/// later pointers right. Caller must have already verified `free_space`
/// covers `cell_bytes.len() + POINTER_LEN`.
pub(super) fn insert_cell_at(
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
pub(super) fn remove_cell_at(
    buf: &mut [u8],
    header_len: usize,
    index: usize,
) -> Result<u16, PinaxError> {
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

pub(super) fn init_leaf(buf: &mut [u8]) -> Result<(), PinaxError> {
    init_slotted(buf, PAGE_TYPE_LEAF)
}

fn leaf_key_at(buf: &[u8], index: usize) -> Result<i64, PinaxError> {
    let offset = pointer_at(buf, LEAF_HEADER_LEN, index)?;
    read_i64(buf, usize::from(offset))
}

pub(super) struct LeafCell {
    pub(super) key: i64,
    pub(super) payload_len: u32,
    pub(super) overflow_first: u32,
    pub(super) local: Vec<u8>,
}

fn leaf_local_len(payload_len: u32, max_local: u32) -> u32 {
    payload_len.min(max_local)
}

pub(super) fn leaf_cell_at(
    buf: &[u8],
    index: usize,
    max_local: u32,
) -> Result<LeafCell, PinaxError> {
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

pub(super) fn leaf_cell_byte_len(
    buf: &[u8],
    index: usize,
    max_local: u32,
) -> Result<u16, PinaxError> {
    let offset = usize::from(pointer_at(buf, LEAF_HEADER_LEN, index)?);
    let payload_len = read_u32(buf, offset + 8)?;
    let local_len = leaf_local_len(payload_len, max_local);
    Ok(u16::try_from(LEAF_CELL_FIXED_LEN).unwrap_or(16) + u16::try_from(local_len).unwrap_or(0))
}

pub(super) fn build_leaf_cell(
    key: i64,
    payload_len: u32,
    overflow_first: u32,
    local: &[u8],
) -> Vec<u8> {
    let mut cell = Vec::with_capacity(LEAF_CELL_FIXED_LEN + local.len());
    cell.extend_from_slice(&key.to_be_bytes());
    cell.extend_from_slice(&payload_len.to_be_bytes());
    cell.extend_from_slice(&overflow_first.to_be_bytes());
    cell.extend_from_slice(local);
    cell
}

/// Binary search a leaf's sorted keys for `key`. `Ok(i)` if present at
/// index `i`; `Err(i)` for the sorted insertion point otherwise.
pub(super) fn leaf_search(buf: &[u8], key: i64) -> Result<Result<usize, usize>, PinaxError> {
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

pub(super) fn init_interior(buf: &mut [u8], rightmost_child: u32) -> Result<(), PinaxError> {
    init_slotted(buf, PAGE_TYPE_INTERIOR)?;
    write_u32(buf, 5, rightmost_child)
}

pub(super) fn interior_rightmost(buf: &[u8]) -> Result<u32, PinaxError> {
    read_u32(buf, 5)
}

pub(super) fn interior_set_rightmost(buf: &mut [u8], child: u32) -> Result<(), PinaxError> {
    write_u32(buf, 5, child)
}

pub(super) fn interior_key_at(buf: &[u8], index: usize) -> Result<i64, PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    read_i64(buf, usize::from(offset))
}

pub(super) fn interior_child_at(buf: &[u8], index: usize) -> Result<u32, PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    read_u32(buf, usize::from(offset) + 8)
}

pub(super) fn interior_set_child_at(
    buf: &mut [u8],
    index: usize,
    child: u32,
) -> Result<(), PinaxError> {
    let offset = pointer_at(buf, INTERIOR_HEADER_LEN, index)?;
    write_u32(buf, usize::from(offset) + 8, child)
}

pub(super) fn build_interior_cell(key: i64, child: u32) -> Vec<u8> {
    let mut cell = Vec::with_capacity(INTERIOR_CELL_LEN);
    cell.extend_from_slice(&key.to_be_bytes());
    cell.extend_from_slice(&child.to_be_bytes());
    cell
}

/// Which child of an interior page an id is referenced from.
pub(super) enum ChildSlot {
    Cell(usize),
    Rightmost,
}

pub(super) fn interior_find_child_slot(buf: &[u8], child_id: u32) -> Result<ChildSlot, PinaxError> {
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
pub(super) fn interior_find_child_for_key(buf: &[u8], key: i64) -> Result<u32, PinaxError> {
    let n = usize::from(num_cells(buf)?);
    for i in 0..n {
        if key < interior_key_at(buf, i)? {
            return interior_child_at(buf, i);
        }
    }
    interior_rightmost(buf)
}
