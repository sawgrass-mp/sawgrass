# Architecture

This is the contract the code follows. Changing it takes an ADR in [`adr/`](adr/) first.

## Goal and budgets

| What | Target |
|---|---|
| Players in one world | 4096 |
| Server tick | 20–30 Hz; p99 tick time under 70% of the tick interval |
| Entities sent to one player | about 100–150 nearby, by priority |
| Downstream per player | about 40 KB/s (320 kbit/s) at 100 visible entities, 20 Hz |
| Downstream for the whole server | about 165 MB/s (1.3 Gbit/s) at 4096 players |

The budgets are estimates until the bots measure them (BOT-2). A pull request that moves tick time or bytes per
player by more than 5% says so with numbers.

## Components

```text
 launcher ──starts──▶ game (Leonida or XCL) + client module ───┐
                                                                │ QUIC
 load-test bots (same protocol, no game) ──────────────────────┤
                                                                ▼
                                                   gateways ×3–4 ──events──▶ world simulation ──▶ PostgreSQL
                                                                                    │
                                                                                    └──▶ web map (admin)
```

- **Gateways** terminate player connections (QUIC through `quinn`: unreliable datagrams for movement, streams
  for events and resource downloads), enforce rate limits, and build each player's snapshot from the world state.
  They scale horizontally because outgoing traffic, not simulation, is the bottleneck.
- **World simulation** is one multithreaded process holding every entity in an ECS. It owns authority decisions,
  runs the game-mode scripts and decides who sees what.
- **Roleplay state** (characters, money, inventory, property) is owned by one writer in the world server, held in
  memory and stored in RocksDB with group commit ([ADR 0003](adr/0003-in-memory-roleplay-state.md)). A change is acknowledged only once it is on disk.
- **PostgreSQL** is the archive, fed from the stored operations asynchronously, for history, admin queries and backups. It never
  decides a balance.
- **Web map** shows live positions for debugging and administration.

## Authority

The game engine cannot run on the server, so physics runs on clients, as in other GTA multiplayer platforms.

- The server assigns an **owner** to every vehicle and NPC, usually the nearest player. The owner simulates it
  and sends its state; the server checks it and relays it.
- Clients decide their own movement and the physics of entities they own. The server validates speed,
  teleports and position against the navmesh.
- **The server alone decides** money, inventory, spawning, death, ownership and anything else with roleplay
  value. A client only asks.
- Every client event is checked against a schema and a rate limit before a script sees it.

## Interest management

- A spatial grid with cells of about 100 m. Each player's visible set is the entities in nearby cells,
  ranked by distance, speed and importance, cut to the per-player byte budget.
- Hysteresis on entering and leaving the set, so entities at the edge do not flicker.
- Far entities update less often than near ones.
- **Routing buckets** (dimensions) separate interiors and instances: entities in different buckets never see each
  other.

## Scaling path

1. One process for everything. Gate: 1000 bots, p99 tick within budget.
2. Gateways in separate processes. Gate: 4096 bots.
3. Only if simulation runs out of CPU: split the map into zones with entity hand-off at the borders. This is the
   most error-prone step and is not started before measurements call for it.

## Scripting

Game modes are **resources**: a folder with a manifest, server scripts and client scripts, in `resources/`. They
use events (server ↔ client), RPC, replicated per-entity state, commands and permissions. The proposed language
is Luau through `mlua` (decision pending, see the roadmap). Client scripts are downloaded from servers and run on
players' machines, so they run in a sandbox with no file, network or process access.

## Client and launcher

- The game runs on PC through one of two **runtimes** ([ADR 0002](adr/0002-two-client-runtimes.md)):
  [Leonida](https://github.com/ItsNotRexed/Leonida) (a static recompilation, runtime `gta6rt`) or
  [XCL](https://github.com/ivall/xcl) (a compatibility layer that runs the console code natively). In-game testing
  starts on whichever boots the game first.
- The client module is a Rust library with a C ABI. A `Runtime` adapter per runtime loads it into the game, installs
  hooks and locates game functions. Functions are found at run time by pattern, per runtime and game version; no
  bytes from the executable are stored in this repository.
- All game access goes through a `GameApi` trait with two implementations: the real one over a runtime adapter,
  and a fake one used by bots and tests. Nothing above `GameApi` knows which runtime it runs on.
- The launcher (Rust and Tauri) detects the player's runtime, checks its build against that runtime's hash
  manifest, installs the signed
  client module, lists servers, downloads server resources and starts the game. It never downloads or unpacks the
  game.

## Anti-cheat

The client is open source and runs on an open recompilation, so nothing on the client can be trusted. Protection
lives on the server: authority as above, event validation, movement checks, per-player statistics and an event
log for admins.
