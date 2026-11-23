# ADR 0003: License the project under MIT OR Apache-2.0

Status: accepted, 2026-11-23 (the maintainer chose this option). Supersedes the licence point of ADR 0001.

This records a licensing decision; it is not legal advice.

## Context

ADR 0001 chose AGPL-3.0. Two things now conflict with it:

- **Storage.** The maintainers put maximum performance above keeping existing code, and storage is chosen by
  measurement among an in-memory ledger with a write-ahead log, SpacetimeDB and PostgreSQL. SpacetimeDB's Rust
  client SDK (`spacetimedb-sdk`) is under the Business Source License 1.1, which cannot be part of an AGPL-3.0
  program. (Its module bindings crate `spacetimedb` is Apache-2.0.) The licence must leave every candidate usable.
- **Client runtimes.** ADR 0002 left open how the client module combines with XCL, which is GPL-2.0-only and not
  compatible with AGPL-3.0. Leonida is GPL-3.0.

Everything in `main` was contributed through the maintainer's account, so the maintainer can relicense it. The
commits authored as `Claude <noreply@anthropic.com>` (shown by GitHub as a separate `claude` contributor) are agent
commits from the maintainer's own sessions.

## Decision

- The project is licensed under **MIT OR Apache-2.0**, at the user's option (`LICENSE-MIT`, `LICENSE-APACHE`),
  for the server, the client module, the launcher and the resources alike.
- Contributions are accepted under the same terms (inbound equals outbound).
- `scripts/check_no_game_data.py` was copied from Leonida and stays GPL-3.0, as its header says. It is a CI tool,
  never shipped or linked into Sawgrass. Its GPL-3.0 text is in `LICENSES/GPL-3.0.txt`. The no-game-data workflow
  that runs it was rewritten for this change, so it is under MIT OR Apache-2.0 like the rest.

## Consequences

- Any storage candidate can be used. Using SpacetimeDB still binds every server operator to its own terms
  (currently one SpacetimeDB instance in production and no database service), whatever our licence; that is
  weighed in the storage decision, and `deny.toml` would need an explicit entry for its SDK.
- The client module can be combined with Leonida (MIT and Apache-2.0 are compatible with GPL-3.0) and with XCL
  (under the MIT option, compatible with GPL-2.0-only). This answers the licensing question in ADR 0002. The client's own source stays MIT OR Apache-2.0, but a client
  distributed combined with XCL is under GPL-2.0 as a whole, and combined with Leonida under GPL-3.0, so whoever
  ships such a combined build to players takes on that copyleft.
- **Given up:** AGPL-3.0 required anyone who ran a modified server for players to publish the changes. Now anyone
  may take Sawgrass, change it and run or sell a closed server or client without sharing anything back.
- Relicensing applies from this change on. Commits and any releases already published under AGPL-3.0 stay
  available under AGPL-3.0.
