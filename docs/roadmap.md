# Roadmap

Stages 0–2 do not need the game and run in parallel with Leonida's work. Stage 3 starts when Leonida boots to a
world with a controllable character.

1. **Stage 0: repository foundation.** Workspace, rules, CI, claim bot, first tasks. Done by maintainers before
   the repository opens to the swarm.
2. **Stage 1: server without the game.** Protocol, gateway, simulation, interest management, bots, web map.
   Gates: 1000 bots on one machine, then 4096.
3. **Stage 2: roleplay base.** PostgreSQL, scripting runtime, base resources (characters, money, inventory, jobs),
   admin tools. Everything tested with bots.
4. **Stage 3: client in the game.** A plugin API in Leonida's runtime (proposed upstream), hooks, sync of
   players, vehicles and damage, in-game UI.
5. **Stage 4: launcher and releases.** Signing, auto-update, server list.
6. **Stage 5: scale.** Real players, zones only if measurements call for them.

## First tasks

These become issues labelled `ready` once this foundation is merged.

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

## Open decisions

- Scripting language: Luau (proposed), WASM or JavaScript.
- Where the community talks (Discord or GitHub only).
