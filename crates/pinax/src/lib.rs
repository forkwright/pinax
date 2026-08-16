//! Facade: pager, buffer pool, B-tree, page format, SQL surface
//! (parser/planner/executor), async API, migration runner, CLI
//! (Decision 1, Decision 2, Decision 6, Decision 7, Decision 10,
//! Decision 12, Decision 14).
//!
//! Empty scaffold reserving this crate's position in the locked dependency
//! graph (`lexis -> hypomnema -> phylaxis -> pinax`). Implementation begins
//! in Phase 01 (pager + buffer pool + B-tree) — see
//! `kanon/projects/pinax/ROADMAP.md`.

#![deny(missing_docs)]
