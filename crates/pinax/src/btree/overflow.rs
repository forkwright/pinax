//! Overflow-chain read/write, and the row spill/reassemble boundary that
//! decides whether an encoded row payload fits local to its leaf cell or
//! needs an overflow chain.
//!
//! WHY this is its own file: split out of `btree.rs` once Phase 01 pushed
//! that file past `RUST/file-too-long`'s 800-line limit, along the section
//! boundary the file already documented internally. See `super`'s module
//! doc for the full split rationale.

use snafu::OptionExt as _;

use super::OVERFLOW_HEADER_LEN;
use super::layout::{LeafCell, build_leaf_cell};
use crate::buffer_pool::BufferPool;
use crate::codec::{read_u32, read_vec, write_u8, write_u32};
use crate::error::{BufferBoundsSnafu, PinaxError};
use crate::page::PAGE_TYPE_OVERFLOW;
use crate::pager::Pager;
use crate::row::Row;

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
    // WHY chunked forward (remainder last), not backward: `read_overflow_chain`
    // reads `remaining_needed.min(chunk_cap)` per page and relies on every
    // page but the LAST holding a full `chunk_cap` bytes — that invariant
    // only holds if the possibly-short remainder chunk is the tail-end
    // chunk in byte order, matching how a chunk_cap-then-remainder split
    // naturally falls out of walking `tail` front-to-back.
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    while start < tail.len() {
        let end = (start + chunk_cap).min(tail.len());
        let chunk = tail.get(start..end).context(BufferBoundsSnafu {
            at: start,
            len: end - start,
            buf_len: tail.len(),
        })?;
        chunks.push(chunk);
        start = end;
    }

    // Link the chain tail-to-head: write the LAST natural chunk (the
    // remainder) first with `next = 0`, and each earlier chunk after it
    // pointing at the page just written — so the final `next_id`, returned
    // as `overflow_first`, is the page holding `tail[0..chunk_cap]`, and
    // reading forward from it visits every chunk in original byte order.
    let mut next_id = 0u32;
    for chunk in chunks.into_iter().rev() {
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
        at: 0usize,
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
pub(super) fn build_row_cell(
    pool: &mut BufferPool,
    key: i64,
    row: &Row,
) -> Result<Vec<u8>, PinaxError> {
    let encoded = row.encode(key)?;
    let (local, overflow_first) = spill_if_needed(pool, &encoded)?;
    let payload_len = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
    Ok(build_leaf_cell(key, payload_len, overflow_first, &local))
}

/// Reassemble a leaf cell's full encoded payload (local bytes plus any
/// overflow chain).
pub(super) fn reassemble(pool: &mut BufferPool, cell: &LeafCell) -> Result<Vec<u8>, PinaxError> {
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
