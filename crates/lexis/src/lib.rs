//! # lexis
//!
//! Language of data for pinax (Decision 5, Decision 14): the strict
//! six-type value system, schema DDL vocabulary, and query-expression AST
//! vocabulary. Leaf crate — no dependency on any other fleet crate.
//!
//! Type discipline is this crate's one job: every newtype validates at
//! construction (`TryFrom`, never an infallible `From` where an invariant
//! exists), fields stay private, and there is no path that produces a
//! value the type system calls valid but the domain calls wrong. No type
//! affinity, no silent coercion, strict mode only — SQLite's type-affinity
//! lookup table is the failure mode this crate exists to rule out.
//!
//! What this crate does NOT do: parse SQL text (Phase 4, hand-rolled
//! recursive descent per Decision 6), plan or execute a query (Phase 4/5),
//! or define the full statement grammar (`SELECT` / `INSERT` / `UPDATE` /
//! `DELETE` / `CREATE TABLE` with joins, subqueries, window functions —
//! Phase 4). This crate defines the vocabulary those phases build with:
//! [`Value`] and [`SqlType`] for data, [`ColumnDef`] and [`TableDef`] for
//! schema DDL, [`Expr`] and its operators for query expressions.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod ast;
mod error;
mod identifier;
mod schema;
mod types;
mod value;

pub use ast::{BinaryOperator, Expr, UnaryOperator};
pub use error::LexisError;
pub use identifier::{ColumnName, TableName};
pub use schema::{ColumnDef, Nullability, TableDef, TextMaxLen};
pub use types::SqlType;
pub use value::{DateTimeValue, RealValue, Value};
