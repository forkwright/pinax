---
scope: pinax repo conventions (design-phase relational storage engine)
---

# AGENTS.md - pinax

Design-phase repo: documentation only. The buildable workspace lands
with Phase 01 (pager / buffer pool / B-tree).

## Rules

- Planning lives in the fleet planning home (kanon `projects/pinax/`);
  this repo carries only what is public and durable.
- Locked decisions in README.md are constraints, not suggestions -
  zero C dependency, async-first, multi-writer MVCC, strict typing.
- Conventional commits; `Gate-Passed:` trailer required on PRs.
