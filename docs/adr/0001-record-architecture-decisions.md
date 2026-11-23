# ADR 0001: Record architecture decisions

Status: accepted, 2026-11-23

## Context

Many contributors, most of them coding agents, work on Sawgrass at once and none of them remembers earlier
discussions. Decisions that live only in chat are lost and get re-argued.

## Decision

Every decision that changes [architecture.md](../architecture.md) gets a numbered file in this folder:
context, decision, consequences. It lands in its own pull request, reviewed by a maintainer, before the code
that depends on it. An ADR is never edited after acceptance; a later ADR supersedes it.

## Decisions already taken

- 4096 players in one world on a dedicated server.
- The client runs on top of Leonida as a module loaded by its runtime.
- One repository: the core plus `resources/` for roleplay game modes, which use only the public scripting API.
- Everything in the repository is in English.
- Licence: AGPL-3.0.

## Consequences

Agents read `docs/adr/` before changing a design and cite the ADR their change follows.
