//! Runtime values for the six-type system (Decision 5).

use std::fmt;

use snafu::ensure;

use crate::error::{LexisError, NanRealSnafu};
use crate::types::SqlType;

/// A validated `REAL` value: an [`f64`] that is never NaN.
///
/// WHY: Decision 5 permits NaN only as a transient computed intermediate —
/// "operators should fail loudly, not propagate NaN silently" — and rejects
/// it "at insert-bind". This newtype IS that insert-bind boundary: a raw
/// `f64` computed mid-expression may be NaN, but the only way to obtain a
/// [`RealValue`] is through [`TryFrom::try_from`], which refuses it.
///
/// WHY `#[repr(transparent)]`: single-field tuple newtype wrapping an
/// `f64`; the representation is guaranteed identical to the wrapped type.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(transparent)]
pub struct RealValue(f64);

impl RealValue {
    /// Read the validated, non-NaN floating-point value.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for RealValue {
    type Error = LexisError;

    /// Validate and construct a [`RealValue`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::NanReal`] if `value` is NaN.
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        ensure!(!value.is_nan(), NanRealSnafu { value });
        Ok(Self(value))
    }
}

impl fmt::Display for RealValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A `DATETIME` value: nanoseconds since the UTC epoch (Decision 5).
///
/// WHY a newtype despite having no rejectable invariant (any `i64` is a
/// valid nanosecond offset): compile-time parameter-swap safety against
/// every other bare `i64` in the domain (row counts, lamport clocks, byte
/// lengths). Because every `i64` is valid, the conversion is honestly
/// infallible — `From`, not `TryFrom` (kanon RUST.md § Validation
/// constructors: "Do not implement `From` ... when [invariants exist]",
/// which implies the converse for the case where none do).
///
/// WHY `#[repr(transparent)]`: single-field tuple newtype wrapping an
/// `i64`; the representation is guaranteed identical to the wrapped type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DateTimeValue(i64);

impl DateTimeValue {
    /// Read the nanosecond UTC epoch offset.
    #[must_use]
    pub fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for DateTimeValue {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<DateTimeValue> for i64 {
    fn from(value: DateTimeValue) -> Self {
        value.0
    }
}

impl fmt::Display for DateTimeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A runtime value inhabiting one of the six types, or `NULL`.
///
/// WHY `NULL` is a variant rather than `Option<Value>` wrapping the rest:
/// SQL's three-valued logic needs `NULL` to flow through the same value
/// channel as typed data (it can be compared, matched by `IS NULL`, and
/// bound to any nullable column regardless of that column's declared
/// type) — `Value` already models "no fixed type" for `Null` via
/// [`Value::sql_type`] returning `None`, so wrapping in `Option` would
/// duplicate that channel rather than clarify it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// SQL `NULL`. Has no fixed type; comparable against any nullable
    /// column regardless of that column's declared type.
    Null,
    /// `INTEGER`: 64-bit signed.
    Integer(i64),
    /// `REAL`: validated non-NaN IEEE-754 double.
    Real(RealValue),
    /// `TEXT`: UTF-8 string.
    Text(String),
    /// `BLOB`: opaque bytes.
    Blob(Vec<u8>),
    /// `BOOLEAN`: `true` / `false`.
    Boolean(bool),
    /// `DATETIME`: nanosecond UTC epoch.
    Datetime(DateTimeValue),
}

impl Value {
    /// The [`SqlType`] this value inhabits, or `None` for `NULL`.
    ///
    /// WHY `None` for `Null`: SQL `NULL` has no fixed type of its own — it
    /// is valid in any nullable column regardless of that column's
    /// declared type (Decision 5).
    #[must_use]
    pub fn sql_type(&self) -> Option<SqlType> {
        match self {
            Self::Null => None,
            Self::Integer(_) => Some(SqlType::Integer),
            Self::Real(_) => Some(SqlType::Real),
            Self::Text(_) => Some(SqlType::Text),
            Self::Blob(_) => Some(SqlType::Blob),
            Self::Boolean(_) => Some(SqlType::Boolean),
            Self::Datetime(_) => Some(SqlType::Datetime),
        }
    }

    /// Whether this value is `NULL`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl TryFrom<f64> for Value {
    type Error = LexisError;

    /// Validate and construct a [`Value::Real`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::NanReal`] if `value` is NaN.
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Ok(Self::Real(RealValue::try_from(value)?))
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}

impl From<DateTimeValue> for Value {
    fn from(value: DateTimeValue) -> Self {
        Self::Datetime(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // WHY explicit: `.prop_filter(...)` below is a `Strategy` trait method
    // called via dot-syntax, which requires the trait in scope even though
    // every other proptest item here is called through a fully-qualified
    // path.
    use proptest::strategy::Strategy as _;

    #[test]
    fn real_value_accepts_finite() {
        let real = RealValue::try_from(3.5).expect("finite value is valid");
        assert!((real.get() - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn real_value_accepts_infinity() {
        // WHY: Decision 5 rejects NaN specifically, not non-finite values
        // in general — infinity is a legitimate computed result.
        assert!(RealValue::try_from(f64::INFINITY).is_ok());
    }

    #[test]
    fn real_value_rejects_nan() {
        let err = RealValue::try_from(f64::NAN).expect_err("NaN must be rejected");
        assert!(matches!(err, LexisError::NanReal { .. }));
    }

    #[test]
    fn value_try_from_f64_rejects_nan() {
        let err = Value::try_from(f64::NAN).expect_err("NaN must be rejected");
        assert!(matches!(err, LexisError::NanReal { .. }));
    }

    #[test]
    fn datetime_value_round_trips_through_i64() {
        let dt = DateTimeValue::from(1_700_000_000_000_000_000_i64);
        assert_eq!(i64::from(dt), 1_700_000_000_000_000_000_i64);
    }

    #[test]
    fn sql_type_maps_each_variant() {
        assert_eq!(Value::Integer(1).sql_type(), Some(SqlType::Integer));
        assert_eq!(
            Value::Real(RealValue::try_from(1.0).expect("valid")).sql_type(),
            Some(SqlType::Real)
        );
        assert_eq!(
            Value::Text(String::from("x")).sql_type(),
            Some(SqlType::Text)
        );
        assert_eq!(Value::Blob(vec![1]).sql_type(), Some(SqlType::Blob));
        assert_eq!(Value::Boolean(true).sql_type(), Some(SqlType::Boolean));
        assert_eq!(
            Value::Datetime(DateTimeValue::from(0)).sql_type(),
            Some(SqlType::Datetime)
        );
    }

    #[test]
    fn null_has_no_sql_type() {
        assert_eq!(Value::Null.sql_type(), None);
        assert!(Value::Null.is_null());
    }

    #[test]
    fn non_null_values_report_is_null_false() {
        assert!(!Value::Integer(0).is_null());
    }

    proptest::proptest! {
        // WHY: `RealValue::try_from` is lexis's one true validated-real
        // boundary — every finite or infinite `f64` must pass, and NaN
        // (in any bit pattern; `is_nan()` is pattern-agnostic) must
        // always be rejected. A fixed set of example values cannot cover
        // this; the property must hold for the whole `f64` domain.
        #[test]
        fn real_value_accepts_iff_not_nan(raw in proptest::num::f64::ANY) {
            let result = RealValue::try_from(raw);
            proptest::prop_assert_eq!(result.is_ok(), !raw.is_nan());
        }

        #[test]
        fn real_value_round_trips_non_nan(raw in proptest::num::f64::ANY.prop_filter(
            "exclude NaN — RealValue::try_from rejects it by construction",
            |value| !value.is_nan(),
        )) {
            let real = RealValue::try_from(raw).expect("non-NaN value is always valid");
            // WHY bit-pattern equality, not `==`: `f64::NAN != f64::NAN`
            // makes `==` unsuitable for a round-trip property, and this
            // path is already filtered to non-NaN, but `-0.0 == 0.0` would
            // also hide a sign-bit round-trip defect that `to_bits` does
            // not.
            proptest::prop_assert_eq!(real.get().to_bits(), raw.to_bits());
        }

        #[test]
        fn datetime_value_round_trips_any_i64(raw in proptest::num::i64::ANY) {
            proptest::prop_assert_eq!(i64::from(DateTimeValue::from(raw)), raw);
        }
    }
}
