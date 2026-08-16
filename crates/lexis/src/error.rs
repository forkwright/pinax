//! Error types for lexis.
//!
//! WHY: one error enum per crate, per kanon RUST.md § Error handling —
//! validation failures across the type system, identifiers, and schema
//! vocabulary all surface through `LexisError` rather than a per-type enum,
//! so a caller matches one surface regardless of which validated
//! constructor rejected the input.

use crate::types::SqlType;

/// Errors raised by lexis's validated constructors and type-checking rules.
///
/// WHY `#[non_exhaustive]`: adding a new validation rule (a new constructor,
/// a new cross-field check) must not be a breaking change for callers that
/// already match on this enum.
#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub(crate)))]
#[non_exhaustive]
pub enum LexisError {
    /// A column or table identifier was empty.
    #[snafu(display("{kind} name must not be empty"))]
    EmptyIdentifier {
        /// The identifier class that was empty (`"column"` or `"table"`).
        kind: &'static str,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A `REAL` value was NaN at the insert-bind boundary.
    ///
    /// WHY: Decision 5 permits NaN only as a transient computed
    /// intermediate; it is rejected the moment a value is bound for
    /// storage. `RealValue::try_from` is that boundary.
    #[snafu(display("REAL value must be finite and non-NaN, got {value}"))]
    NanReal {
        /// The rejected value.
        value: f64,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A `TEXT` column's declared maximum length was zero.
    #[snafu(display("TEXT max length must be greater than zero"))]
    TextMaxLenZero {
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A maximum length was declared on a non-`TEXT` column.
    #[snafu(display("max length is only valid for TEXT columns, got {sql_type}"))]
    TextMaxLenOnNonText {
        /// The column's actual declared type.
        sql_type: SqlType,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// Two `SqlType`s cannot be compared without an explicit `CAST`.
    ///
    /// WHY: Decision 5 permits INTEGER↔REAL comparison (numerics unify) and
    /// same-type comparison; every other pairing is a type error, not a
    /// silent coercion.
    #[snafu(display("cannot compare {left} with {right}: explicit CAST required"))]
    IncomparableTypes {
        /// The left-hand operand's type.
        left: SqlType,
        /// The right-hand operand's type.
        right: SqlType,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A table was declared with zero columns.
    #[snafu(display("table `{table}` must declare at least one column"))]
    EmptyColumnList {
        /// The table's name.
        table: String,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A table declared the same column name more than once.
    #[snafu(display("table `{table}` declares column `{column}` more than once"))]
    DuplicateColumn {
        /// The table's name.
        table: String,
        /// The column name that repeated.
        column: String,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// `Value::Null` was bound to a `NOT NULL` column.
    #[snafu(display("column `{column}` is NOT NULL"))]
    NullNotAllowed {
        /// The column's name.
        column: String,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A value's runtime type did not match its column's declared type.
    ///
    /// WHY: Decision 5's entire point — no type affinity, no implicit
    /// conversion. `expected` and `actual` are always unequal when this
    /// variant is constructed.
    #[snafu(display("column `{column}` expects {expected}, got {actual}"))]
    TypeMismatch {
        /// The column's name.
        column: String,
        /// The column's declared type.
        expected: SqlType,
        /// The value's actual type.
        actual: SqlType,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },

    /// A `TEXT` value exceeded its column's declared maximum length.
    #[snafu(display(
        "column `{column}` exceeds max length {max_len} (got {actual_len} characters)"
    ))]
    TextTooLong {
        /// The column's name.
        column: String,
        /// The column's declared maximum length.
        max_len: u32,
        /// The value's actual character length.
        actual_len: usize,
        /// Error creation location.
        #[snafu(implicit)]
        location: snafu::Location,
    },
}
