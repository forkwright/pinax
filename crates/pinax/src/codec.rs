//! Bounds-checked big-endian byte access for page buffers.
//!
//! WHY this module exists: the conformance bar forbids indexing/slicing a
//! buffer directly (`buf[at]`, `&buf[a..b]`) because that panics on an
//! out-of-bounds offset — exactly the failure mode a corrupt or truncated
//! page produces. Every accessor here goes through `.get()`/`.get_mut()`
//! and returns [`FatalError::BufferBounds`] instead of panicking, so a
//! malformed page turns into a typed error the caller can classify rather
//! than an unwind.

use snafu::OptionExt as _;

use crate::error::{BufferBoundsSnafu, PinaxError};

/// Read `N` bytes at `at` from `buf` without slicing.
///
/// WHY safe despite `copy_from_slice`: `buf.get(at..at + n)` returning
/// `Some` guarantees the returned slice's length is exactly `n` (that is
/// what a valid range slice means), so the length precondition
/// `copy_from_slice` requires always holds by construction — the
/// fallible part (the offset being out of bounds) is already handled by
/// the `.get()` call above it.
pub(crate) fn read_bytes<const N: usize>(buf: &[u8], at: usize) -> Result<[u8; N], PinaxError> {
    let buf_len = buf.len();
    let slice = buf.get(at..at + N).context(BufferBoundsSnafu {
        at,
        len: N,
        buf_len,
    })?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

/// Write `bytes` into `buf` starting at `at` without slicing.
pub(crate) fn write_bytes(buf: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), PinaxError> {
    let buf_len = buf.len();
    let slot = buf
        .get_mut(at..at + bytes.len())
        .context(BufferBoundsSnafu {
            at,
            len: bytes.len(),
            buf_len,
        })?;
    slot.copy_from_slice(bytes);
    Ok(())
}

/// Read a single byte without indexing.
pub(crate) fn read_u8(buf: &[u8], at: usize) -> Result<u8, PinaxError> {
    Ok(read_bytes::<1>(buf, at)?[0])
}

/// Write a single byte without indexing.
pub(crate) fn write_u8(buf: &mut [u8], at: usize, value: u8) -> Result<(), PinaxError> {
    write_bytes(buf, at, &[value])
}

/// Read a big-endian `u16` without indexing.
pub(crate) fn read_u16(buf: &[u8], at: usize) -> Result<u16, PinaxError> {
    Ok(u16::from_be_bytes(read_bytes::<2>(buf, at)?))
}

/// Write a big-endian `u16` without indexing.
pub(crate) fn write_u16(buf: &mut [u8], at: usize, value: u16) -> Result<(), PinaxError> {
    write_bytes(buf, at, &value.to_be_bytes())
}

/// Read a big-endian `u32` without indexing.
pub(crate) fn read_u32(buf: &[u8], at: usize) -> Result<u32, PinaxError> {
    Ok(u32::from_be_bytes(read_bytes::<4>(buf, at)?))
}

/// Write a big-endian `u32` without indexing.
pub(crate) fn write_u32(buf: &mut [u8], at: usize, value: u32) -> Result<(), PinaxError> {
    write_bytes(buf, at, &value.to_be_bytes())
}

/// Read a big-endian `u64` without indexing.
pub(crate) fn read_u64(buf: &[u8], at: usize) -> Result<u64, PinaxError> {
    Ok(u64::from_be_bytes(read_bytes::<8>(buf, at)?))
}

/// Write a big-endian `u64` without indexing.
pub(crate) fn write_u64(buf: &mut [u8], at: usize, value: u64) -> Result<(), PinaxError> {
    write_bytes(buf, at, &value.to_be_bytes())
}

/// Read a big-endian `i64` without indexing.
pub(crate) fn read_i64(buf: &[u8], at: usize) -> Result<i64, PinaxError> {
    Ok(i64::from_be_bytes(read_bytes::<8>(buf, at)?))
}

/// Read `len` bytes starting at `at` into an owned, growable `Vec<u8>`
/// without slicing — the runtime-length counterpart to
/// [`read_bytes`]'s const-generic fixed length.
pub(crate) fn read_vec(buf: &[u8], at: usize, len: usize) -> Result<Vec<u8>, PinaxError> {
    let buf_len = buf.len();
    let slice = buf
        .get(at..at + len)
        .context(BufferBoundsSnafu { at, len, buf_len })?;
    Ok(slice.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FatalError;

    #[test]
    fn u16_round_trips() {
        let mut buf = vec![0u8; 8];
        write_u16(&mut buf, 2, 0xABCD).expect("in bounds");
        assert_eq!(read_u16(&buf, 2).expect("in bounds"), 0xABCD);
    }

    #[test]
    fn u32_round_trips() {
        let mut buf = vec![0u8; 8];
        write_u32(&mut buf, 0, 0xDEAD_BEEF).expect("in bounds");
        assert_eq!(read_u32(&buf, 0).expect("in bounds"), 0xDEAD_BEEF);
    }

    #[test]
    fn u64_round_trips() {
        let mut buf = vec![0u8; 8];
        write_u64(&mut buf, 0, 0x0123_4567_89AB_CDEF).expect("in bounds");
        assert_eq!(read_u64(&buf, 0).expect("in bounds"), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn i64_round_trips_negative() {
        // WHY built via `extend_from_slice` rather than a `write_i64`
        // helper: no page write ever places an `i64` at a fixed offset
        // into an existing buffer (row/cell encoding always appends to a
        // growing `Vec` — see `row.rs`'s module docs), so `codec` has no
        // `write_i64` to call; this test constructs the expected on-disk
        // layout the same way production code does.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(-42i64).to_be_bytes());
        assert_eq!(read_i64(&buf, 0).expect("in bounds"), -42);
    }

    #[test]
    fn read_out_of_bounds_errors() {
        let buf = vec![0u8; 4];
        let err = read_u64(&buf, 0).expect_err("8 bytes at offset 0 exceeds a 4-byte buffer");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::BufferBounds { .. }
            }
        ));
    }

    #[test]
    fn write_out_of_bounds_errors() {
        let mut buf = vec![0u8; 4];
        let err = write_u32(&mut buf, 2, 1).expect_err("4 bytes at offset 2 exceeds 4-byte buffer");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::BufferBounds { .. }
            }
        ));
    }

    #[test]
    fn u8_round_trips() {
        let mut buf = vec![0u8; 2];
        write_u8(&mut buf, 1, 7).expect("in bounds");
        assert_eq!(read_u8(&buf, 1).expect("in bounds"), 7);
    }
}
