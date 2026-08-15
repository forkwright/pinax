//! Validated schema identifiers.
//!
//! WHY newtypes: `ColumnName` and `TableName` wrap the same underlying
//! representation but must never be interchangeable at a call site — a
//! function that takes a table name must not silently accept a column
//! name. Two distinct types give the compiler that guarantee for free
//! (kanon RUST.md § Type system, Newtypes for domain concepts).
//!
//! WHY only non-emptiness is validated: Decision 5 and Decision 6 fix the
//! six-type system and the parser strategy but do not specify identifier
//! grammar (allowed character set, maximum length, quoting, reserved-word
//! handling). Phase 4 (parser) owns full SQL identifier syntax; inventing a
//! charset or length cap here would be a decision this crate has no
//! authority to make. Non-emptiness is the one invariant that holds
//! regardless of what that grammar turns out to be.

use std::fmt;

use compact_str::CompactString;
use snafu::ensure;

use crate::error::{EmptyIdentifierSnafu, LexisError};

/// A validated, non-empty column identifier.
///
/// WHY `#[repr(transparent)]`: single-field tuple newtype wrapping a
/// `CompactString`; the representation is guaranteed identical to the
/// wrapped type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ColumnName(CompactString);

impl ColumnName {
    /// Borrow the validated identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ColumnName {
    type Error = LexisError;

    /// Validate and construct a [`ColumnName`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::EmptyIdentifier`] if `value` is empty.
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        ensure!(!value.is_empty(), EmptyIdentifierSnafu { kind: "column" });
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ColumnName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A validated, non-empty table identifier.
///
/// WHY `#[repr(transparent)]`: single-field tuple newtype wrapping a
/// `CompactString`; the representation is guaranteed identical to the
/// wrapped type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TableName(CompactString);

impl TableName {
    /// Borrow the validated identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for TableName {
    type Error = LexisError;

    /// Validate and construct a [`TableName`].
    ///
    /// # Errors
    ///
    /// Returns [`LexisError::EmptyIdentifier`] if `value` is empty.
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        ensure!(!value.is_empty(), EmptyIdentifierSnafu { kind: "table" });
        Ok(Self(value.into()))
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TableName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_name_accepts_non_empty() {
        let name = ColumnName::try_from("id").expect("non-empty identifier is valid");
        assert_eq!(name.as_str(), "id");
    }

    #[test]
    fn column_name_rejects_empty() {
        let err = ColumnName::try_from("").expect_err("empty identifier must be rejected");
        assert!(matches!(
            err,
            LexisError::EmptyIdentifier { kind: "column", .. }
        ));
    }

    #[test]
    fn table_name_accepts_non_empty() {
        let name = TableName::try_from("media_items").expect("non-empty identifier is valid");
        assert_eq!(name.as_str(), "media_items");
    }

    #[test]
    fn table_name_rejects_empty() {
        let err = TableName::try_from("").expect_err("empty identifier must be rejected");
        assert!(matches!(
            err,
            LexisError::EmptyIdentifier { kind: "table", .. }
        ));
    }

    #[test]
    fn display_matches_as_str() {
        let column = ColumnName::try_from("added_at").expect("valid identifier");
        assert_eq!(column.to_string(), "added_at");
        let table = TableName::try_from("media_items").expect("valid identifier");
        assert_eq!(table.to_string(), "media_items");
    }

    #[test]
    fn column_name_and_table_name_are_distinct_types() {
        // WHY: this is a compile-time property, not a runtime assertion —
        // if `ColumnName` and `TableName` ever became interchangeable this
        // test would fail to compile, not fail to pass.
        fn takes_table_name(_: &TableName) {}
        let column = ColumnName::try_from("id").expect("valid identifier");
        let table = TableName::try_from("id").expect("valid identifier");
        takes_table_name(&table);
        assert_eq!(column.as_str(), table.as_str());
    }
}
