# Contributing

This is a fleet repo of a single-operator organization. External PRs are
welcome for discussion but the roadmap is planning-driven; open an issue
first so intent is aligned before code is written.

## Process

- PRs target `main` via squash merge.
- Every non-bot PR carries a `Gate-Passed:` commit trailer produced by the
  fleet's local gate; CI verifies its presence.
- Conventional commits: `type(scope): description`.

## Planning

Roadmap, phase plans, and state tracking live in the fleet planning home
(`forkwright/kanon` under `projects/`), not in this repo. This repo holds
what is buildable and shippable; planning history stays out of it.
