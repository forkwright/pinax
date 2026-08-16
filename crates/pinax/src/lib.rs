//! Facade: page format, pager, buffer pool, and B+tree (Decision 1,
//! Decision 2, Decision 14). SQL surface (parser/planner/executor), async
//! API, migration runner, and CLI land in later phases per ROADMAP.md.
//!
//! Phase 01 (this phase) lands a durable, checksummed, ordered
//! integer-keyed key/value store: [`Database`] is the entry point.
//!
//! ```
//! use lexis::Value;
//! use pinax::{Database, PageSize, Row};
//!
//! # fn main() -> Result<(), pinax::PinaxError> {
//! let dir = tempfile::tempdir().expect("tempdir");
//! let path = dir.path().join("example.pinax");
//! let mut db = Database::create(&path, PageSize::DEFAULT)?;
//! db.insert(1, &Row::new(vec![Value::Text("hello".to_owned())]))?;
//! assert!(db.get(1)?.is_some());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod btree;
mod buffer_pool;
mod codec;
mod database;
mod error;
mod page;
mod pager;
mod row;

pub use database::{DEFAULT_BUFFER_POOL_CAPACITY, Database};
pub use error::{FatalError, PermanentError, PinaxError};
pub use page::PageSize;
pub use row::Row;
