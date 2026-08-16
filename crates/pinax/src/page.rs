//! Page format: size validation, layout constants, and the XxHash3-64
//! checksum (PLAN.md Decision 2).
//!
//! Every page pinax writes — meta, leaf, interior, overflow — is exactly
//! [`PageSize::bytes`] long and carries an 8-byte XxHash3-64 checksum in
//! its trailing reserved region. `reserved_bytes` is fixed at 8 here
//! because Phase 01 has no encryption path (Decision 2's 48-byte reserved
//! region is the encrypted-tablespace case; Phase 06 adds it).

use snafu::ensure;

use crate::codec::{read_u64, write_u64};
use crate::error::{InvalidPageSizeSnafu, PinaxError};

/// Trailing bytes on every page reserved for the XxHash3-64 checksum.
///
/// WHY 8, unconditionally: Decision 2 reserves 8 bytes when the tablespace
/// is unencrypted and 48 when it carries per-page AEAD (32-byte nonce +
/// 16-byte tag, checksum omitted because the AEAD tag already authenticates
/// the page). Phase 01 has no encryption path — `phylaxis` (the crate that
/// owns AEAD, per Decision 14) is still an empty scaffold — so only the
/// 8-byte unencrypted layout exists yet.
pub(crate) const CHECKSUM_LEN: usize = 8;

/// [`CHECKSUM_LEN`] restated as `u32` for page-size arithmetic.
///
/// WHY a second constant rather than a cast: the "no `as` casts" rule
/// means every `usize -> u32` conversion needs `try_from`, and doing that
/// at every call site for a value that is always `8` adds noise without
/// adding safety. One `const` restatement keeps both units available
/// without a fallible conversion anywhere.
const CHECKSUM_LEN_U32: u32 = 8;

/// [`CHECKSUM_LEN`] as `u32`, for `btree`'s page-buffer arithmetic.
pub(crate) fn checksum_len_u32() -> u32 {
    CHECKSUM_LEN_U32
}

/// [`CHECKSUM_LEN`] restated as `u16`, for the same reason as
/// [`CHECKSUM_LEN_U32`]: `btree`'s slotted-page offsets are `u16` (every
/// offset within one page fits — see [`PageSize::max_local`]'s doc on why
/// `usable_space` never exceeds it).
const CHECKSUM_LEN_U16: u16 = 8;

/// [`CHECKSUM_LEN`] as `u16`, for `btree`'s slotted-page offset arithmetic.
pub(crate) fn checksum_len_u16() -> u16 {
    CHECKSUM_LEN_U16
}

/// The smallest page size Decision 2 permits.
///
/// WHY 4096 and not SQLite's 512: no fleet target runs on 512-byte-sector
/// hardware (PLAN.md Decision 2) — raising the floor to match present SSD
/// and ext4/xfs block granularity eliminates a dead code path rather than
/// preserving compatibility with hardware nothing in the fleet uses.
pub(crate) const MIN_PAGE_SIZE: u32 = 4096;

/// The largest page size Decision 2 permits, matching SQLite's ceiling.
pub(crate) const MAX_PAGE_SIZE: u32 = 65536;

/// Fixed size of each meta-page slot's checksummed region, independent of
/// the database's configured [`PageSize`].
///
/// WHY fixed rather than `page_size`-sized: `Pager::open` must learn the
/// configured page size FROM the meta page before it can compute any
/// `page_size`-relative file offset. If the meta region's own size (and
/// therefore slot 1's file offset) depended on that not-yet-known value,
/// opening a database would need to guess before it could verify. Pinning
/// the meta region to the format's own floor size — the meta page's actual
/// content (magic, version, page size, txn id, root, page count) is well
/// under 100 bytes regardless of the configured data page size — makes
/// bootstrap independent of the value it discovers.
///
/// WHY a literal restatement of [`MIN_PAGE_SIZE`] rather than
/// `u64::from(MIN_PAGE_SIZE)`: the widening `u32 -> u64` conversion is
/// lossless, but `From`'s trait method is not yet usable inside a `const`
/// initializer on this toolchain (rust-lang/rust#143874) — the same
/// "restate rather than convert" reasoning [`CHECKSUM_LEN_U32`] and
/// [`CHECKSUM_LEN_U16`] above already apply to sidestep a fallible/`as`
/// conversion for a value that never changes independently of its source.
pub(crate) const META_SLOT_LEN: u64 = 4096;

/// Total file offset before the first data page begins.
///
/// WHY two [`META_SLOT_LEN`] slots rather than one: PLAN.md Decision 1
/// requires the copy-on-write B+tree to "survive crash-and-reopen"
/// (ROADMAP.md Phase 01). A single meta page rewritten in place is exposed
/// to a torn write mid-commit; ping-ponging between two checksummed slots
/// (see `pager::commit`) means a torn write on the slot being written
/// leaves the OTHER slot — still carrying the prior, fully durable
/// commit — as a valid fallback on reopen.
pub(crate) const META_REGION_LEN: u64 = META_SLOT_LEN * 2;

/// The lowest page id a data (leaf/interior/overflow) page may use.
///
/// Page ids 0 and 1 are the two meta slots; data pages begin at 2.
pub(crate) const FIRST_DATA_PAGE_ID: u32 = 2;

/// Sentinel `root_page_id` meaning "the tree is empty".
///
/// WHY 0 is safe as a sentinel despite page id 0 being real (meta slot A):
/// no data page is ever assigned id 0 or 1 — [`FIRST_DATA_PAGE_ID`] starts
/// the bump allocator at 2 — so a `root_page_id` of 0 can never collide
/// with an actual data page reference.
pub(crate) const EMPTY_TREE_ROOT: u32 = 0;

/// Page type byte identifying a leaf B+tree page.
pub(crate) const PAGE_TYPE_LEAF: u8 = 1;
/// Page type byte identifying an interior B+tree page.
pub(crate) const PAGE_TYPE_INTERIOR: u8 = 2;
/// Page type byte identifying an overflow chain page.
pub(crate) const PAGE_TYPE_OVERFLOW: u8 = 3;

/// A validated, database-lifetime-immutable page size (Decision 2).
///
/// WHY `TryFrom` and not `From`: the value must be one of the five sizes
/// Decision 2 locks (4096/8192/16384/32768/65536) — an arbitrary `u32`
/// invalidates the invariant, so construction is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageSize(u32);

impl PageSize {
    /// The fleet default (Decision 2).
    pub const DEFAULT: Self = Self(4096);

    /// The configured page size in bytes.
    #[must_use]
    pub fn bytes(self) -> u32 {
        self.0
    }

    /// [`Self::bytes`] as `usize`, for allocating a page-sized buffer.
    ///
    /// WHY the fallback is `usize::MAX` and not `0`: every locked page
    /// size (Decision 2: 4096..=65536) fits `usize` on any platform this
    /// fleet targets (64-bit Linux) — the fallback path is unreachable in
    /// practice. `usize::MAX` fails loudly (an allocation of that size
    /// aborts) rather than silently producing a zero-length buffer a
    /// caller could mistake for a valid empty page.
    #[must_use]
    pub(crate) fn bytes_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }

    /// Bytes available for page content after the trailing checksum
    /// region (Decision 2's `usable_space`).
    #[must_use]
    pub(crate) fn usable_space(self) -> u32 {
        self.0 - CHECKSUM_LEN_U32
    }

    /// The largest locally-stored cell payload before it must spill to an
    /// overflow chain (Decision 2: `max_local = usable_space - 35`).
    ///
    /// WHY 35 is not re-derived: Decision 2 states pinax "copies SQLite's
    /// formula" verbatim (citing `turso/core/storage/btree.rs:8194-8220`,
    /// a design prior, not vendored code) rather than deriving it from
    /// pinax's own cell layout, so 35 is the locked constant, not computed
    /// from `LEAF_CELL_FIXED_LEN` or any other pinax-specific figure.
    #[must_use]
    pub(crate) fn max_local(self) -> u32 {
        self.usable_space().saturating_sub(35)
    }
}

impl TryFrom<u32> for PageSize {
    type Error = PinaxError;

    /// Validate and construct a [`PageSize`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::PermanentError::InvalidPageSize`] unless
    /// `value` is exactly one of 4096, 8192, 16384, 32768, or 65536.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        ensure!(
            value.is_power_of_two() && (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&value),
            InvalidPageSizeSnafu { requested: value }
        );
        Ok(Self(value))
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Compute the XxHash3-64 checksum over `buf[..buf.len() - CHECKSUM_LEN]`.
///
/// WHY infallible: `buf.get(..content_len)` falls back to the whole buffer
/// on a too-short slice rather than failing, and XxHash3 itself has no
/// error path — there is no condition this function could report.
pub(crate) fn compute_checksum(buf: &[u8]) -> u64 {
    let content_len = buf.len().saturating_sub(CHECKSUM_LEN);
    let content = buf.get(..content_len).unwrap_or(buf);
    xxhash_rust::xxh3::xxh3_64(content)
}

/// Stamp `buf`'s trailing [`CHECKSUM_LEN`] bytes with the checksum of
/// everything before them.
pub(crate) fn stamp_checksum(buf: &mut [u8]) -> Result<(), PinaxError> {
    let checksum = compute_checksum(buf);
    let at = buf.len().saturating_sub(CHECKSUM_LEN);
    write_u64(buf, at, checksum)
}

/// Verify `buf`'s trailing checksum against its recomputed content
/// checksum. Returns `Ok(())` on match, `Err` describing the mismatch
/// otherwise. The caller attaches the page id.
///
/// WHY `unwrap_or(0)` below is safe rather than a masked failure:
/// [`compute_checksum`] is infallible; only `read_u64` can fail here, and
/// only on a buffer shorter than [`CHECKSUM_LEN`], which every page-sized
/// buffer in this crate never is. Coalescing that unreachable case to `0`
/// still produces the correct outcome (an undersized buffer reads as a
/// checksum mismatch, not a silent pass) rather than requiring this
/// function to propagate a `PinaxError` for a condition that cannot occur
/// given how every caller constructs its buffers.
pub(crate) fn verify_checksum(buf: &[u8]) -> Result<(), (u64, u64)> {
    let expected = read_u64(buf, buf.len().saturating_sub(CHECKSUM_LEN)).unwrap_or(0);
    let actual = compute_checksum(buf);
    if expected == actual {
        Ok(())
    } else {
        Err((expected, actual))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_accepts_all_locked_values() {
        for value in [4096, 8192, 16384, 32768, 65536] {
            assert_eq!(PageSize::try_from(value).expect("valid").bytes(), value);
        }
    }

    #[test]
    fn page_size_rejects_non_power_of_two() {
        let err = PageSize::try_from(5000).expect_err("not a valid page size");
        assert!(matches!(
            err,
            PinaxError::Permanent {
                source: crate::error::PermanentError::InvalidPageSize { .. }
            }
        ));
    }

    #[test]
    fn page_size_rejects_below_minimum() {
        assert!(PageSize::try_from(512).is_err());
    }

    #[test]
    fn page_size_rejects_above_maximum() {
        assert!(PageSize::try_from(131_072).is_err());
    }

    #[test]
    fn default_is_4096() {
        assert_eq!(PageSize::default().bytes(), 4096);
    }

    #[test]
    fn max_local_matches_decision_2_formula() {
        let page_size = PageSize::DEFAULT;
        assert_eq!(page_size.usable_space(), 4096 - 8);
        assert_eq!(page_size.max_local(), 4096 - 8 - 35);
    }

    #[test]
    fn checksum_round_trips() {
        let mut buf = vec![0xAB_u8; 4096];
        stamp_checksum(&mut buf).expect("buffer at least CHECKSUM_LEN long");
        assert!(verify_checksum(&buf).is_ok());
    }

    #[test]
    fn checksum_detects_flipped_byte() {
        let mut buf = vec![0xAB_u8; 4096];
        stamp_checksum(&mut buf).expect("buffer at least CHECKSUM_LEN long");
        if let Some(byte) = buf.get_mut(10) {
            *byte ^= 0xFF;
        }
        assert!(verify_checksum(&buf).is_err());
    }
}
