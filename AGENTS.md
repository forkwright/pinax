---
scope: pinax repo conventions (relational storage engine; Phase 01 implemented)
defers_to: kanon standards for universal engineering policy
tightens: no crates land ahead of the roadmap's current phase (kanon projects/pinax/ROADMAP.md)
---

# AGENTS.md - pinax

Phase 01 (pager / buffer pool / B-tree) is implemented, in lexis and
pinax. hypomnema and phylaxis reserve their workspace position for
later phases.

## Rules

- Planning lives in the fleet planning home (kanon `projects/pinax/`);
  this repo carries only what is public and durable.
- Locked decisions in README.md are constraints, not suggestions -
  zero C dependency, async-first, multi-writer MVCC, strict typing.
- Conventional commits; `Gate-Passed:` trailer required on PRs.
