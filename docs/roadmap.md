# Roadmap

Stages 0–2 do not need the game and run in parallel with the runtimes' work. Stage 3 starts when VIctory or XCL,
whichever comes first, runs the game to a world with a controllable character (ADR 0004).

1. **Stage 0: repository foundation.** Workspace, rules, CI, claim bot, first tasks. Done by maintainers before
   the repository opens to the swarm.
2. **Stage 1: server without the game.** Protocol, gateway, simulation, interest management, bots, web map.
   Gates: 1000 bots on one machine, then 4096.
3. **Stage 2: roleplay base.** Roleplay state in server memory, stored in RocksDB, with a PostgreSQL archive
   (ADR 0003), scripting runtime, base resources (characters, money, inventory, jobs), admin tools. Everything
   tested with bots.
4. **Stage 3: client in the game.** A runtime adapter for the first runtime that gets there (VIctory or XCL), a
   module-loading API proposed upstream, hooks, sync of players, vehicles and damage, in-game UI. The other
   runtime's adapter follows. Runtime-independent client code (sync and prediction over the fake `GameApi`) can
   start earlier.
5. **Stage 4: launcher and releases.** Signing, auto-update, server list.
6. **Stage 5: scale.** Real players, zones only if measurements call for them.

## First tasks

Each is an issue that starts in `triage`. A maintainer labels it `ready` when it may start; until then it cannot be
claimed.

| ID | Task | Size |
|---|---|---|
| NET-1 | Packet format and quantization of positions and rotations, with precision tests | S |
| NET-2 | Client-to-gateway connection over `quinn`: handshake, protocol version, datagrams | M |
| IM-1 | Spatial grid and visible set with hysteresis, plus a benchmark with 4096 points | M |
| SRV-1 | Tick loop on an ECS with tick-duration metrics | M |
| BOT-1 | Headless bot that walks between waypoints and speaks the protocol | M |
| BOT-2 | Run N bots and report tick p50/p99 and bytes per player | S |
| DB-1 | Character and wallet schema with a transaction ledger | S |
| OPS-1 | Benchmark job in CI that fails on a regression over 5% | M |
| OPS-2 | Markdown link check in CI | S |
| OPS-3 | Fail CI when `sawgrass-protocol` changes without a `PROTOCOL_VERSION` bump | S |
| DOC-1 | Wire format section in `architecture.md` once NET-1 lands | S |
| OPS-4 | Review agent in comment-only mode (first task after this foundation is merged; see below) | L |

## Review agent

A separate agent reviews pull requests and, in time, merges the low-risk ones. The reviewer is a Claude session
run by a maintainer (in the maintainers' Claude project or on a maintainer's machine), acting through that
maintainer's GitHub access. It is never the session that wrote the pull request.

| Tier | Covers | Who merges |
|---|---|---|
| 1 | Docs, tests, roleplay scripts in `resources/`, small changes to non-critical code | The agent, when CI is green, the pull request closes a task a maintainer labelled `ready`, it changes only files in the task's scope, and the diff is small |
| 2 | Server core, protocol, dependencies | The agent posts a review and a verdict; a maintainer merges |
| 3 | Launcher and auto-update, client module, CI and repository settings, agent rules, the reviewer itself | Maintainers only |

Safeguards:

- The reviewer reads its rules from `main` and only reads pull request code; it never builds or runs it.
- Author and reviewer are different sessions with different instructions. They may share a maintainer's GitHub
  account, so a review is a comment, not a GitHub approval (GitHub does not let an account approve its own pull
  request).
- Anyone with triage rights stops an automatic merge with a label; agent merges are squash commits, easy to revert.
- It started in comment-only mode. Since 2026-11-24, by the maintainer's decision, it merges the maintainer's own
  pull requests of every tier once CI is green and its review has no blocking finding; the table above still
  applies to pull requests from other accounts. `hold` on a pull request or its task stops its merge.

How it works today: a maintainer starts a reviewer session, which follows
[maintainers.md](maintainers.md#review-agent). The tier, the task link and the size limit come from deterministic
rules in `tools/reviewer/policy.py`, never from the model. Until automatic merging is enabled, only docs, tests,
`resources/` and `README.md` count as tier 1; all other code is tier 2. Reviews happen only while such a session
runs.

`.github/workflows/review.yml` and `tools/reviewer/review.py` can run the same review from GitHub Actions with an
API key. They stay dormant (they do nothing without the `ANTHROPIC_API_KEY` secret) unless the maintainers decide
to use them.

## Open decisions

- Scripting language: Luau (proposed), WASM or JavaScript.
- Where the community talks (Discord or GitHub only).
