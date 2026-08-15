//! Row encoding: `lexis::Value` tuples to and from the byte payload a
//! B+tree leaf cell carries.
//!
//! WHY `lexis::Value` and not opaque bytes: `lexis`'s own exit criteria
//! (`crates/lexis/Cargo.toml`) names this directly — "Phase 1 pager adopts
//! `lexis::Value` for on-disk row encoding". The full typed-row storage
//! format (per-column type checking against a `TableDef`, `NULL` bitmaps
//! keyed by schema) is Phase 4/5 territory once `CREATE TABLE` exists; this
//! module owns only the byte-level encode/decode of a self-describing
//! value tuple, which is what a schema-less Phase 01 B+tree can use.
//!
//! Format (fixed-width, not SQLite's varints): a `u32` BE column count,
//! then each value as a 1-byte type tag followed by a type-specific
//! payload. WHY fixed-width: Decision 1 says pinax "owns its on-disk
//! encoding" rather than targeting SQLite format compatibility, and
//! fixed-width fields keep this codec's bounds-checked-read discipline
//! simple — a varint reader adds a variable-length-decode loop for no
//! benefit Phase 01 needs.
//!
//! WHY `Row::encode` builds into a growing `Vec<u8>` via `extend_from_slice`
//! rather than routing through `codec`'s bounds-checked writers: those
//! writers exist to make a WRITE INTO A FIXED-SIZE PAGE BUFFER fail
//! typed instead of panicking on an out-of-range offset. Appending to a
//! `Vec` has no such offset — `extend_from_slice` cannot go out of bounds —
//! so the bounds-checking discipline has nothing to add on the write side
//! here. The read side still goes through `codec`, because decoding reads
//! at caller-supplied offsets into a buffer whose length is not statically
//! known to be sufficient.

use lexis::{DateTimeValue, RealValue, Value};

use crate::codec::{read_i64, read_u8, read_u32, read_u64, read_vec};
use crate::error::{FatalError, PermanentError, PinaxError};

const TAG_NULL: u8 = 0;
const TAG_INTEGER: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BLOB: u8 = 4;
const TAG_BOOLEAN: u8 = 5;
const TAG_DATETIME: u8 = 6;

/// A B+tree row: an ordered tuple of [`lexis::Value`], keyed externally by
/// the B+tree's `i64` key (Decision 5's typed values, Decision 1's
/// integer-keyed B+tree).
#[derive(Debug, Clone, PartialEq)]
pub struct Row(Vec<Value>);

impl Row {
    /// Wrap a value tuple as a [`Row`].
    #[must_use]
    pub fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    /// The row's values in column order.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.0
    }

    /// Encode this row to its on-disk byte payload.
    ///
    /// # Errors
    ///
    /// Returns [`PermanentError::PayloadTooLarge`] only in the
    /// astronomically unlikely case a `TEXT`/`BLOB` value's byte length, or
    /// the row's own column count, does not fit in a `u32`; `key` is
    /// carried through to name the offending row in that error.
    pub(crate) fn encode(&self, key: i64) -> Result<Vec<u8>, PinaxError> {
        let mut buf = Vec::new();
        let count = too_large_as_u32(self.0.len(), key)?;
        buf.extend_from_slice(&count.to_be_bytes());
        for value in &self.0 {
            encode_value(&mut buf, value, key)?;
        }
        Ok(buf)
    }

    /// Decode a row from bytes produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`FatalError::InvalidRowEncoding`] if `buf` does not decode
    /// as a value of this module's on-disk format — see that variant's
    /// docs for why this indicates a deeper problem than bad input.
    pub(crate) fn decode(buf: &[u8]) -> Result<Self, PinaxError> {
        let mut at = 0usize;
        let count = read_u32(buf, at)?;
        at += 4;
        let mut values = Vec::new();
        for _ in 0..count {
            let (value, next) = decode_value(buf, at)?;
            values.push(value);
            at = next;
        }
        Ok(Self(values))
    }
}

fn too_large_as_u32(len: usize, key: i64) -> Result<u32, PinaxError> {
    u32::try_from(len).map_err(|_source| PinaxError::Permanent {
        source: PermanentError::PayloadTooLarge {
            key,
            encoded_len: len,
            location: snafu::Location::new(file!(), line!(), column!()),
        },
    })
}

fn invalid_encoding(reason: &'static str) -> PinaxError {
    PinaxError::Fatal {
        source: FatalError::InvalidRowEncoding {
            reason,
            location: snafu::Location::new(file!(), line!(), column!()),
        },
    }
}

fn encode_value(buf: &mut Vec<u8>, value: &Value, key: i64) -> Result<(), PinaxError> {
    match value {
        Value::Null => buf.push(TAG_NULL),
        Value::Integer(v) => {
            buf.push(TAG_INTEGER);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        Value::Real(v) => {
            buf.push(TAG_REAL);
            buf.extend_from_slice(&v.get().to_bits().to_be_bytes());
        }
        Value::Text(s) => {
            buf.push(TAG_TEXT);
            push_len_prefixed(buf, s.as_bytes(), key)?;
        }
        Value::Blob(b) => {
            buf.push(TAG_BLOB);
            push_len_prefixed(buf, b, key)?;
        }
        Value::Boolean(v) => {
            buf.push(TAG_BOOLEAN);
            buf.push(u8::from(*v));
        }
        Value::Datetime(v) => {
            buf.push(TAG_DATETIME);
            buf.extend_from_slice(&v.get().to_be_bytes());
        }
    }
    Ok(())
}

fn push_len_prefixed(buf: &mut Vec<u8>, bytes: &[u8], key: i64) -> Result<(), PinaxError> {
    let len = too_large_as_u32(bytes.len(), key)?;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
    Ok(())
}

fn decode_value(buf: &[u8], at: usize) -> Result<(Value, usize), PinaxError> {
    let tag = read_u8(buf, at)?;
    let mut at = at + 1;
    let value = match tag {
        TAG_NULL => Value::Null,
        TAG_INTEGER => {
            let v = read_i64(buf, at)?;
            at += 8;
            Value::Integer(v)
        }
        TAG_REAL => {
            let bits = read_u64(buf, at)?;
            at += 8;
            let real = RealValue::try_from(f64::from_bits(bits))
                .map_err(|_source| invalid_encoding("REAL payload decoded to NaN"))?;
            Value::Real(real)
        }
        TAG_TEXT => {
            let (bytes, next) = read_len_prefixed(buf, at)?;
            at = next;
            let text = String::from_utf8(bytes)
                .map_err(|_source| invalid_encoding("TEXT payload was not valid UTF-8"))?;
            Value::Text(text)
        }
        TAG_BLOB => {
            let (bytes, next) = read_len_prefixed(buf, at)?;
            at = next;
            Value::Blob(bytes)
        }
        TAG_BOOLEAN => {
            let raw = read_u8(buf, at)?;
            at += 1;
            Value::Boolean(raw != 0)
        }
        TAG_DATETIME => {
            let v = read_i64(buf, at)?;
            at += 8;
            Value::Datetime(DateTimeValue::from(v))
        }
        _ => return Err(invalid_encoding("unrecognized value type tag")),
    };
    Ok((value, at))
}

fn read_len_prefixed(buf: &[u8], at: usize) -> Result<(Vec<u8>, usize), PinaxError> {
    let len = read_u32(buf, at)?;
    let len_usize = usize::try_from(len).unwrap_or(usize::MAX);
    let start = at + 4;
    let bytes = read_vec(buf, start, len_usize)?;
    Ok((bytes, start + len_usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_type() {
        let row = Row::new(vec![
            Value::Null,
            Value::Integer(-7),
            Value::Real(RealValue::try_from(3.5).expect("finite")),
            Value::Text("hello".to_owned()),
            Value::Blob(vec![1, 2, 3]),
            Value::Boolean(true),
            Value::Datetime(DateTimeValue::from(42)),
        ]);
        let encoded = row.encode(1).expect("encodes");
        let decoded = Row::decode(&encoded).expect("decodes");
        assert_eq!(row, decoded);
    }

    #[test]
    fn round_trips_empty_row() {
        let row = Row::new(vec![]);
        let encoded = row.encode(1).expect("encodes");
        let decoded = Row::decode(&encoded).expect("decodes");
        assert_eq!(row, decoded);
    }

    #[test]
    fn round_trips_empty_text_and_blob() {
        let row = Row::new(vec![Value::Text(String::new()), Value::Blob(vec![])]);
        let encoded = row.encode(1).expect("encodes");
        let decoded = Row::decode(&encoded).expect("decodes");
        assert_eq!(row, decoded);
    }

    #[test]
    fn round_trips_large_text() {
        let text = "x".repeat(10_000);
        let row = Row::new(vec![Value::Text(text.clone())]);
        let encoded = row.encode(1).expect("encodes");
        let decoded = Row::decode(&encoded).expect("decodes");
        assert_eq!(decoded.values(), &[Value::Text(text)]);
    }

    #[test]
    fn decode_rejects_unrecognized_tag() {
        let buf = [0u8, 0, 0, 1, 0xFF];
        let err = Row::decode(&buf).expect_err("tag 0xFF is not a known type");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::InvalidRowEncoding { .. }
            }
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let mut buf = vec![0, 0, 0, 1, TAG_TEXT];
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&[0xFF, 0xFE]);
        let err = Row::decode(&buf).expect_err("0xFF 0xFE is not valid UTF-8");
        assert!(matches!(
            err,
            PinaxError::Fatal {
                source: FatalError::InvalidRowEncoding { .. }
            }
        ));
    }

    #[test]
    fn values_accessor_matches_constructor() {
        let row = Row::new(vec![Value::Integer(1)]);
        assert_eq!(row.values(), &[Value::Integer(1)]);
    }
}
