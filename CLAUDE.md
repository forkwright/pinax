<!--
scope: pinax repo conventions (relational storage engine; Phase 01 implemented)
defers_to: kanon standards for universal engineering policy
tightens: no crates land ahead of the roadmap's current phase; planning lives in kanon projects/pinax, not here
-->

# CLAUDE.md - pinax

## At a glance

Fleet relational storage engine. Four-crate workspace (lexis,
hypomnema, phylaxis, pinax); Phase 01 (pager / buffer pool / B-tree)
is implemented, in lexis and pinax. hypomnema and phylaxis reserve
their workspace position for later phases. Do not add crates here
without the current phase's plan in hand.

## Standards

Fleet standards come from forkwright/kanon `crates/basanos/standards/`.
Apply now that code has landed: RUST.md, STORAGE.md, STORAGE-TIERS.md,
CRATE-SHAPE.md.

## Planning

Roadmap / state / phase plans live in kanon `projects/pinax/` - not in
this repo. Read them before proposing work. The phase ladder runs:
spec -> pager+B-tree -> WAL+transactions -> MVCC -> SQL surface ->
joins/indexes -> encryption -> changelog emission -> async API ->
migrations -> audit-chain -> harmonia cutover -> fleet migration.

## Boundaries

- Always: keep this repo's docs consistent with kanon planning; one
  fact, one place - link, do not restate volatile detail.
- Never: introduce a C dependency (the zero-C constraint is a locked
  decision, not a preference); copy SQLite type-affinity semantics.
