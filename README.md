# pinax

*πίναξ - the board, tablet, register. The Pinakes of Callimachus (Library
of Alexandria, ~250 BC) was the first systematic structured index of
knowledge.*

Relational storage engine for the forkwright fleet, designed from scratch
in Rust. The Tier-1 storage primitive: the answer to "what is." Replaces
SQLite for fleet consumers whose state is tabular, transactional, and
aggregation-heavy.

**Status:** Phase 01 (pager / buffer pool / B-tree) implemented — a
checksummed, copy-on-write, integer-keyed B+tree behind an LRU-evicting
buffer pool. The phased roadmap and state tracking live in the fleet
planning home; `lexis` (the strict six-type value system) landed first,
as the vocabulary Phase 01 stores.

## Locked design decisions

- **Ground-up Rust, zero C dependency.** No libsqlite3-sys, no bindgen,
  no cc build step.
- **Async-first.** Tokio-native surface is primary; sync is a wrapper
  over async, not the reverse.
- **Multi-writer MVCC from day one.** Single-writer is a SQLite legacy
  constraint, not an inherited requirement.
- **Per-page encryption always available.** XChaCha20-Poly1305 default,
  AES-256-GCM opt-in, per-tablespace keys via HKDF. A core feature, not
  a fork or extension.
- **Changelog as a storage primitive.** Every committed transaction
  produces a causal-ordered changeset with (site_id, lamport_clock) -
  audit-chain, replication, and downstream fact-ingest consume it;
  nothing bolts on via triggers.
- **Strict typing, no coercion.** INTEGER is INTEGER; TEXT is TEXT.
  Type affinity is a silent-corruption footgun this engine does not copy.
- **No SQLite on-disk-format compatibility.** Pinax owns its format.
- **Vector and full-text indexing delegated.** `CREATE VECTOR INDEX` /
  `CREATE FTS INDEX` are syntax here; the implementations belong to
  [heurema](https://github.com/forkwright/heurema).

## Planned consumers

harmonia (media library), daimon (memory), hamma (histos coordination
state), dioptron (tenant registry), eventually the kanon forge itself.
First production migration target: harmonia's `apotheke` crate.

## License

PolyForm Shield 1.0.0 (code) - see [LICENSE](LICENSE). Documentation
under CC BY-NC-ND 4.0 - see [LICENSE-DOCS](LICENSE-DOCS).
