//! The strict six-type system (Decision 5).
//!
//! WHY: SQLite's type affinity silently coerces between declared and
//! bound types (`SQLITE_AFF_NUMERIC` accepting TEXT, for example). Pinax
//! rejects that entirely — a column declares exactly one of these six
//! types, values must match it or be `NULL` in a nullable column, and
//! every implicit conversion is a type error, not a coercion. `NUMERIC`,
//! `DECIMAL`, `DATE`, `TIME`, and `JSON` are explicitly rejected as column
//! types by Decision 5 and must never be added here.

use std::fmt;

use snafu::ensure;

use crate::error::{IncomparableTypesSnafu, LexisError};

/// The six fixed SQL types a column may declare (Decision 5).
///
/// WHY `#[non_exhaustive]`: this set is locked by Decision 5, but the enum
/// still carries the fleet-wide public-enum convention so a hypothetical
/// future addition is not a breaking change for exhaustive-matching callers
/// outside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SqlType {
    /// 64-bit signed integer. No unsigned variant.
    Integer,
    /// IEEE-754 double.
    Real,
    /// UTF-8 text, optionally length-bounded by the declaring column.
    Text,
    /// Opaque bytes, no encoding implied.
    Blob,
    /// `true` / `false`, distinct from `Integer`.
    Boolean,
    /// 64-bit nanosecond UTC epoch.
    Datetime,
}

impl SqlType {
    /// Check whether two types may be compared without an explicit `CAST`.
    ///
    /// WHY: Decision 5 permits exactly one cross-type pairing —
    /// `Integer`↔`Real`, because "numerics unify to REAL for the compare".
    /// Every other cross-type pairing (INTEGER vs TEXT, and so on) is a
    /// type error. Same-type pairs are always comparable.
    ///
    /// This checks type compatibility only; it does not perform the
    /// comparison itself. Runtime value comparison — including the
    /// mantissa-exactness rule for `INTEGER`↔`REAL` and three-valued `NULL`
    /// propagation — is executor behavior assigned to the pinax facade
    /// (Phase 4/5 planner and executor), not this crate's vocabulary.
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::IncomparableTypes`] if `self` and `other` are
    /// different types other than the `Integer`/`Real` pairing.
    #[must_use]
    pub fn check_comparable(self, other: Self) -> Result<(), LexisError> {
        let compatible = self == other
            || matches!(
                (self, other),
                (Self::Integer, Self::Real) | (Self::Real, Self::Integer)
            );
        ensure!(
            compatible,
            IncomparableTypesSnafu {
                left: self,
                right: other,
            }
        );
        Ok(())
    }
}

impl fmt::Display for SqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::Boolean => "BOOLEAN",
            Self::Datetime => "DATETIME",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_comparable_accepts_identical_types() {
        assert!(SqlType::Text.check_comparable(SqlType::Text).is_ok());
    }

    #[test]
    fn check_comparable_accepts_integer_real_either_direction() {
        assert!(SqlType::Integer.check_comparable(SqlType::Real).is_ok());
        assert!(SqlType::Real.check_comparable(SqlType::Integer).is_ok());
    }

    #[test]
    fn check_comparable_rejects_integer_text() {
        let err = SqlType::Integer
            .check_comparable(SqlType::Text)
            .expect_err("INTEGER vs TEXT must be rejected");
        assert!(matches!(err, LexisError::IncomparableTypes { .. }));
    }

    #[test]
    fn check_comparable_rejects_boolean_integer() {
        // WHY: SQLite conflates 0/1 with true/false; Decision 5 explicitly
        // keeps BOOLEAN distinct from INTEGER.
        let err = SqlType::Boolean
            .check_comparable(SqlType::Integer)
            .expect_err("BOOLEAN vs INTEGER must be rejected");
        assert!(matches!(err, LexisError::IncomparableTypes { .. }));
    }

    #[test]
    fn display_matches_sql_keyword() {
        assert_eq!(SqlType::Integer.to_string(), "INTEGER");
        assert_eq!(SqlType::Real.to_string(), "REAL");
        assert_eq!(SqlType::Text.to_string(), "TEXT");
        assert_eq!(SqlType::Blob.to_string(), "BLOB");
        assert_eq!(SqlType::Boolean.to_string(), "BOOLEAN");
        assert_eq!(SqlType::Datetime.to_string(), "DATETIME");
    }

    /// WHY a hand-written strategy rather than a derive: `SqlType` has no
    /// `Arbitrary` impl (adding one would pull `proptest-derive` into a
    /// library dependency for a six-variant enum); enumerating the fixed
    /// set directly is both simpler and, per Decision 5, exhaustive by
    /// construction — no seventh variant can appear.
    fn any_sql_type() -> impl proptest::strategy::Strategy<Value = SqlType> {
        proptest::prop_oneof![
            proptest::strategy::Just(SqlType::Integer),
            proptest::strategy::Just(SqlType::Real),
            proptest::strategy::Just(SqlType::Text),
            proptest::strategy::Just(SqlType::Blob),
            proptest::strategy::Just(SqlType::Boolean),
            proptest::strategy::Just(SqlType::Datetime),
        ]
    }

    proptest::proptest! {
        // WHY: Decision 5's comparison rule is inherently symmetric ("a
        // compares with b" cannot be true in one direction and false in
        // the other for a same-type-or-numeric-unify rule) — this
        // property must hold across all 36 ordered pairs, not just the
        // ones a hand-picked example set happens to cover.
        #[test]
        fn check_comparable_is_symmetric(
            left in any_sql_type(),
            right in any_sql_type(),
        ) {
            proptest::prop_assert_eq!(
                left.check_comparable(right).is_ok(),
                right.check_comparable(left).is_ok(),
            );
        }

        #[test]
        fn check_comparable_always_accepts_identical_types(sql_type in any_sql_type()) {
            proptest::prop_assert!(sql_type.check_comparable(sql_type).is_ok());
        }
    }
}
