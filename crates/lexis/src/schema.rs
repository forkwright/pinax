//! Schema DDL vocabulary: columns and tables (Decision 5, Decision 14).

use std::collections::HashSet;
use std::num::NonZeroU32;

use snafu::{OptionExt, ensure};

use crate::error::{
    DuplicateColumnSnafu, EmptyColumnListSnafu, LexisError, NullNotAllowedSnafu,
    TextMaxLenOnNonTextSnafu, TextMaxLenZeroSnafu, TextTooLongSnafu, TypeMismatchSnafu,
};
use crate::identifier::{ColumnName, TableName};
use crate::types::SqlType;
use crate::value::Value;

/// Whether a column accepts `NULL`.
///
/// WHY no [`Default`] impl: Decision 5 requires "explicit `NULL` / `NOT
/// NULL` declaration ... omitting the annotation is a parse error." A
/// `Default` would give silent-nullable-by-omission exactly the semantics
/// the decision rejects — every caller must name one of the two variants.
///
/// ```compile_fail
/// // Nullability deliberately has no Default: omitting the annotation
/// // must be a parse error, not a silent NULLABLE.
/// let _n: lexis::Nullability = Default::default();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Nullability {
    /// Column accepts `NULL`.
    Nullable,
    /// Column rejects `NULL`.
    NotNull,
}

/// A validated, non-zero `TEXT` column length bound (`TEXT(n)`).
///
/// WHY `#[repr(transparent)]`: single-field tuple newtype wrapping a
/// [`NonZeroU32`]; the representation is guaranteed identical to the
/// wrapped type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TextMaxLen(NonZeroU32);

impl TextMaxLen {
    /// Read the validated, non-zero length bound.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl TryFrom<u32> for TextMaxLen {
    type Error = LexisError;

    /// Validate and construct a [`TextMaxLen`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::TextMaxLenZero`] if `value` is zero.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        NonZeroU32::new(value)
            .map(Self)
            .context(TextMaxLenZeroSnafu)
    }
}

/// A single column's schema declaration (Decision 5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnDef {
    name: ColumnName,
    sql_type: SqlType,
    nullability: Nullability,
    text_max_len: Option<TextMaxLen>,
}

impl ColumnDef {
    /// Validate and construct a [`ColumnDef`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::TextMaxLenOnNonText`] if `text_max_len` is
    /// `Some` and `sql_type` is not [`SqlType::Text`].
    #[must_use]
    pub fn new(
        name: ColumnName,
        sql_type: SqlType,
        nullability: Nullability,
        text_max_len: Option<TextMaxLen>,
    ) -> Result<Self, LexisError> {
        ensure!(
            text_max_len.is_none() || sql_type == SqlType::Text,
            TextMaxLenOnNonTextSnafu { sql_type }
        );
        Ok(Self {
            name,
            sql_type,
            nullability,
            text_max_len,
        })
    }

    /// The column's validated name.
    #[must_use]
    pub fn name(&self) -> &ColumnName {
        &self.name
    }

    /// The column's declared type.
    #[must_use]
    pub fn sql_type(&self) -> SqlType {
        self.sql_type
    }

    /// The column's nullability.
    #[must_use]
    pub fn nullability(&self) -> Nullability {
        self.nullability
    }

    /// The column's declared `TEXT` length bound, if any.
    #[must_use]
    pub fn text_max_len(&self) -> Option<TextMaxLen> {
        self.text_max_len
    }

    /// Check a value against this column's declared type and nullability.
    ///
    /// WHY this is lexis's job: Decision 5's entire point is "values
    /// inserted into a column must be of that type or NULL (if nullable).
    /// Implicit conversions are rejected with a type error" — this method
    /// is that rule, expressed once so every future caller (the pager's
    /// row encoder, the executor's INSERT/UPDATE path) enforces it
    /// identically instead of re-implementing it.
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::NullNotAllowed`] if `value` is `NULL` and
    /// this column is [`Nullability::NotNull`].
    ///
    /// Returns [`LexisError::TypeMismatch`] if `value`'s type does not
    /// exactly match this column's declared type.
    ///
    /// Returns [`LexisError::TextTooLong`] if `value` is `TEXT`, this
    /// column declares a `text_max_len`, and the value's character count
    /// exceeds it.
    #[must_use]
    pub fn check_value(&self, value: &Value) -> Result<(), LexisError> {
        let actual_type = match value {
            Value::Null => {
                ensure!(
                    self.nullability == Nullability::Nullable,
                    NullNotAllowedSnafu {
                        column: self.name.as_str(),
                    }
                );
                return Ok(());
            }
            Value::Integer(_) => SqlType::Integer,
            Value::Real(_) => SqlType::Real,
            Value::Text(_) => SqlType::Text,
            Value::Blob(_) => SqlType::Blob,
            Value::Boolean(_) => SqlType::Boolean,
            Value::Datetime(_) => SqlType::Datetime,
        };

        ensure!(
            actual_type == self.sql_type,
            TypeMismatchSnafu {
                column: self.name.as_str(),
                expected: self.sql_type,
                actual: actual_type,
            }
        );

        // WHY no `as` cast: `u32::try_from(len)` is itself the bound check
        // for a `usize` too large for `u32` — a failed conversion means
        // `len` cannot possibly fit under `max_len`, so `is_ok_and` folds
        // "too large to convert" and "converts but exceeds the bound" into
        // one comparison without ever discarding precision silently.
        if let (Value::Text(text), Some(max_len)) = (value, self.text_max_len) {
            let len = text.chars().count();
            let within_bound = u32::try_from(len).is_ok_and(|actual| actual <= max_len.get());
            ensure!(
                within_bound,
                TextTooLongSnafu {
                    column: self.name.as_str(),
                    max_len: max_len.get(),
                    actual_len: len,
                }
            );
        }

        Ok(())
    }
}

/// A table's schema: name plus an ordered, duplicate-free column list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableDef {
    name: TableName,
    columns: Vec<ColumnDef>,
}

impl TableDef {
    /// Validate and construct a [`TableDef`].
    ///
    /// WHY these two invariants beyond Decision 5's text: a table with
    /// zero columns, or two columns sharing a name, cannot exist in any
    /// relational model — this is relational-model correctness, not an
    /// invented pinax-specific rule.
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::EmptyColumnList`] if `columns` is empty.
    ///
    /// Returns [`LexisError::DuplicateColumn`] if any two columns share a
    /// name.
    #[must_use]
    pub fn new(name: TableName, columns: Vec<ColumnDef>) -> Result<Self, LexisError> {
        ensure!(
            !columns.is_empty(),
            EmptyColumnListSnafu {
                table: name.as_str(),
            }
        );

        let mut seen: HashSet<&str> = HashSet::new();
        for column in &columns {
            ensure!(
                seen.insert(column.name().as_str()),
                DuplicateColumnSnafu {
                    table: name.as_str(),
                    column: column.name().as_str(),
                }
            );
        }

        Ok(Self { name, columns })
    }

    /// The table's validated name.
    #[must_use]
    pub fn name(&self) -> &TableName {
        &self.name
    }

    /// The table's columns, in declaration order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Look up a column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns
            .iter()
            .find(|column| column.name().as_str() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_column() -> ColumnDef {
        ColumnDef::new(
            ColumnName::try_from("id").expect("valid identifier"),
            SqlType::Integer,
            Nullability::NotNull,
            None,
        )
        .expect("INTEGER NOT NULL with no length bound is always valid")
    }

    #[test]
    fn text_max_len_accepts_positive() {
        let bound = TextMaxLen::try_from(255).expect("positive length is valid");
        assert_eq!(bound.get(), 255);
    }

    #[test]
    fn text_max_len_rejects_zero() {
        let err = TextMaxLen::try_from(0).expect_err("zero length must be rejected");
        assert!(matches!(err, LexisError::TextMaxLenZero { .. }));
    }

    #[test]
    fn column_def_rejects_max_len_on_non_text() {
        let bound = TextMaxLen::try_from(10).expect("valid bound");
        let err = ColumnDef::new(
            ColumnName::try_from("id").expect("valid identifier"),
            SqlType::Integer,
            Nullability::NotNull,
            Some(bound),
        )
        .expect_err("max length on INTEGER must be rejected");
        assert!(matches!(err, LexisError::TextMaxLenOnNonText { .. }));
    }

    #[test]
    fn column_def_accepts_max_len_on_text() {
        let bound = TextMaxLen::try_from(255).expect("valid bound");
        let column = ColumnDef::new(
            ColumnName::try_from("title").expect("valid identifier"),
            SqlType::Text,
            Nullability::Nullable,
            Some(bound),
        )
        .expect("max length on TEXT is valid");
        assert_eq!(column.text_max_len(), Some(bound));
    }

    #[test]
    fn check_value_rejects_null_on_not_null_column() {
        let column = id_column();
        let err = column
            .check_value(&Value::Null)
            .expect_err("NULL on NOT NULL column must be rejected");
        assert!(matches!(err, LexisError::NullNotAllowed { .. }));
    }

    #[test]
    fn check_value_accepts_null_on_nullable_column() {
        let column = ColumnDef::new(
            ColumnName::try_from("note").expect("valid identifier"),
            SqlType::Text,
            Nullability::Nullable,
            None,
        )
        .expect("valid column");
        assert!(column.check_value(&Value::Null).is_ok());
    }

    #[test]
    fn check_value_rejects_type_mismatch() {
        let column = id_column();
        let err = column
            .check_value(&Value::Text(String::from("nope")))
            .expect_err("TEXT into INTEGER column must be rejected");
        assert!(matches!(
            err,
            LexisError::TypeMismatch {
                expected: SqlType::Integer,
                actual: SqlType::Text,
                ..
            }
        ));
    }

    #[test]
    fn check_value_accepts_matching_type() {
        let column = id_column();
        assert!(column.check_value(&Value::Integer(42)).is_ok());
    }

    #[test]
    fn check_value_rejects_text_over_max_len() {
        let bound = TextMaxLen::try_from(3).expect("valid bound");
        let column = ColumnDef::new(
            ColumnName::try_from("code").expect("valid identifier"),
            SqlType::Text,
            Nullability::NotNull,
            Some(bound),
        )
        .expect("valid column");
        let err = column
            .check_value(&Value::Text(String::from("abcd")))
            .expect_err("4 characters must exceed a bound of 3");
        assert!(matches!(err, LexisError::TextTooLong { .. }));
    }

    #[test]
    fn check_value_counts_characters_not_bytes() {
        // WHY: multi-byte UTF-8 must not be penalized for byte length when
        // the declared bound is a character count.
        let bound = TextMaxLen::try_from(2).expect("valid bound");
        let column = ColumnDef::new(
            ColumnName::try_from("code").expect("valid identifier"),
            SqlType::Text,
            Nullability::NotNull,
            Some(bound),
        )
        .expect("valid column");
        // "\u{1F600}\u{1F600}" is two grapheme-adjacent scalars, 8 bytes.
        assert!(
            column
                .check_value(&Value::Text(String::from("\u{1F600}\u{1F600}")))
                .is_ok()
        );
    }

    #[test]
    fn table_def_rejects_empty_columns() {
        let err = TableDef::new(TableName::try_from("t").expect("valid identifier"), vec![])
            .expect_err("zero columns must be rejected");
        assert!(matches!(err, LexisError::EmptyColumnList { .. }));
    }

    #[test]
    fn table_def_rejects_duplicate_column_names() {
        let err = TableDef::new(
            TableName::try_from("t").expect("valid identifier"),
            vec![id_column(), id_column()],
        )
        .expect_err("duplicate column name must be rejected");
        assert!(matches!(err, LexisError::DuplicateColumn { .. }));
    }

    #[test]
    fn table_def_accepts_distinct_columns_and_looks_up_by_name() {
        let title = ColumnDef::new(
            ColumnName::try_from("title").expect("valid identifier"),
            SqlType::Text,
            Nullability::Nullable,
            None,
        )
        .expect("valid column");
        let table = TableDef::new(
            TableName::try_from("t").expect("valid identifier"),
            vec![id_column(), title],
        )
        .expect("distinct columns are valid");
        assert_eq!(table.columns().len(), 2);
        assert_eq!(
            table.column("title").map(ColumnDef::sql_type),
            Some(SqlType::Text)
        );
        assert!(table.column("missing").is_none());
    }
}
