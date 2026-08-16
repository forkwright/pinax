//! Path-copying mutation result and propagation: descending to a leaf,
//! propagating a split/replace result back up through ancestor interior
//! pages, and finalizing (or collapsing) the new root.
//!
//! WHY this is its own file: split out of `btree.rs` once Phase 01 pushed
//! that file past `RUST/file-too-long`'s 800-line limit, along the section
//! boundary the file already documented internally. See `super`'s module
//! doc for the full split rationale.

use snafu::OptionExt as _;

use super::layout::{
    self, ChildSlot, build_interior_cell, init_interior, interior_child_at,
    interior_find_child_for_key, interior_find_child_slot, interior_key_at, interior_rightmost,
    interior_set_child_at, interior_set_rightmost, num_cells,
};
use super::{BufferBoundsSnafu, INTERIOR_CELL_LEN, INTERIOR_HEADER_LEN, POINTER_LEN};
use crate::buffer_pool::BufferPool;
use crate::codec::read_u8;
use crate::error::PinaxError;
use crate::page::{PAGE_TYPE_INTERIOR, PAGE_TYPE_LEAF};
use crate::pager::Pager;

// WHY `Copy`: every field is a trivially-copyable primitive (`u32`/`i64`),
// and `finalize_root` below consumes its `NodeResult` argument at each call
// site's last use — `Copy` lets it take that argument by value without
// clippy flagging an avoidable move, matching the by-value idiom Rust
// prefers for small POD-shaped enums.
#[derive(Clone, Copy)]
pub(super) enum NodeResult {
    Replaced(u32),
    Split {
        left: u32,
        right: u32,
        separator_key: i64,
    },
}

pub(super) fn descend_path(
    pool: &mut BufferPool,
    root: u32,
    key: i64,
) -> Result<Vec<u32>, PinaxError> {
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
pub(super) fn interior_entries(buf: &[u8]) -> Result<(Vec<i64>, Vec<u32>), PinaxError> {
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
        layout::insert_cell_at(&mut buf, INTERIOR_HEADER_LEN, i, &cell)?;
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

pub(super) fn apply_result_to_interior(
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
        len: 1usize,
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

pub(super) fn finalize_root(pool: &mut BufferPool, result: NodeResult) -> Result<u32, PinaxError> {
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
            layout::insert_cell_at(&mut buf, INTERIOR_HEADER_LEN, 0, &cell)?;
            let id = pool.allocate_page_id();
            pool.put_new(id, buf)?;
            Ok(id)
        }
    }
}

/// Collapse an interior root with zero separator keys to its sole
/// (rightmost) child, defensively — see module docs on why nothing in
/// Phase 01's current delete path actually produces this shape yet.
pub(super) fn collapse_root_if_needed(pool: &mut BufferPool, root: u32) -> Result<u32, PinaxError> {
    let buf = pool.get(root)?;
    if read_u8(&buf, 0)? != PAGE_TYPE_INTERIOR {
        return Ok(root);
    }
    if num_cells(&buf)? == 0 {
        return interior_rightmost(&buf);
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::page::PageSize;
    use crate::pager::Pager;

    fn pool(dir: &TempDir, capacity: usize) -> BufferPool {
        let path = dir.path().join("db.pinax");
        let pager = Pager::create(&path, PageSize::DEFAULT).expect("create");
        BufferPool::new(pager, capacity).expect("valid capacity")
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
        layout::init_leaf(&mut leaf_buf).expect("init leaf");
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
        layout::init_leaf(&mut leaf_buf).expect("init leaf");
        pool.put_new(leaf_id, leaf_buf).expect("put leaf");

        let result = collapse_root_if_needed(&mut pool, leaf_id).expect("no-op on a leaf root");
        assert_eq!(result, leaf_id);
    }
}
