# CLAUDE.md - pinax

## At a glance

Design-phase repo for the fleet's relational storage engine. No code
yet - the tree is documentation and scaffold. Do not add crates here
without the Phase 01 plan in hand.

## Standards

Fleet standards come from forkwright/kanon `crates/basanos/standards/`.
Most relevant once code lands: RUST.md, STORAGE.md, STORAGE-TIERS.md,
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
