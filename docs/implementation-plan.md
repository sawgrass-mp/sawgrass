# Implementation plan: from the measured choices to the server

The comparisons in [`research/`](research/) chose a stack, and the load-test bots measured the server as it is
([BOT-3](research/load-test-bots/README.md)). Most of those choices are not in the server yet. This page puts them
together into one target and orders the work so that what buys the most tick time and reliability lands first. It
changes no decision: every row below cites the report or ADR that made it, and [architecture.md](architecture.md)
stays the contract. Task IDs are new and continue the series in [roadmap.md](roadmap.md).

## Where the server stands

| Part | Chosen | In `sawgrass-server` on `main` (2026-11-24) |
|---|---|---|
| Transport | QUIC through quinn ([NET-3](research/network-transport/README.md)) | Yes, one in-process gateway |
| Async runtime | Tokio multi-thread ([SRV-5](research/allocator-runtime/README.md)) | Yes |
| Allocator | jemalloc ([SRV-5](research/allocator-runtime/README.md), confirmed by BOT-3) | No: glibc malloc |
| Serialization | postcard ([NET-4](research/packet-serialization/README.md), confirmed by NET-8) | No: hand-written format |
| Snapshots | Delta against the last acknowledged snapshot ([NET-5](research/packet-compression/README.md)) | No: whole snapshots |
| Visible sets | Dense 50 m grid, bitset sets over dense slots ([IM-2](research/visibility-index/README.md)) | No: hashed 100 m grid, sorted sets, ids never reused |
| Tick model | hecs, per-player work in parallel on rayon, 30 Hz ([SRV-4](research/tick-model/README.md)) | No: one thread, 20 Hz |
| Roleplay state | One writer in memory, RocksDB with group commit ([ADR 0003](adr/0003-in-memory-roleplay-state.md)) | No: `sawgrass-db` is the PostgreSQL ledger only |
| Scripting | Luau through mlua, deadlines and memory caps ([ADR 0005](adr/0005-luau-scripting.md)) | No |
| Voice | Our own relay, Opus over QUIC ([NET-7](research/voice-chat/README.md)) | No |

### What limits it today

BOT-3 on one 4-core machine, server on 2 cores: in a dense city (1.5 km square, about 150 players in view) the
tick overran its 50 ms at 2,000 bots (p99 52 ms) and the server dropped bots at 3,000. The tick thread, not the
network, is the limit. Its profile at 2,000 bots in that crowd:

| Part of the tick | Share | Fixed by |
|---|---:|---|
| Memory allocation | 27% | SRV-6 (jemalloc), NET-9 (buffers reused across ticks) |
| Visible sets | 26% | IM-4, IM-5 (dense slots, dense grid, bitsets), SRV-7 (parallel) |
| Waking the gateway, once per datagram | 22% | NET-9 (one hand-off per tick) |
| Copying states out of hash maps | 8% | IM-4 (states in a slot-indexed array) |
| Encoding and handing datagrams to quinn | 6% | SRV-7 (encoding in parallel), NET-10 and NET-11 (fewer, smaller datagrams) |

## Target

```text
            ┌──────────── world server process ─────────────────────────────────────────────────┐
 clients ──▶│ gateway (tokio, quinn)          tick thread, 30 Hz                                 │
  (QUIC)    │  per-connection tasks ──inbox──▶ 1 ingest + validate (serial, slot order)          │
            │  latest state per player         2 grid rebuild (dense 50 m cells)                 │
            │                                  3 per player, rayon: visible set (bitset),        │
            │  outbox drain task ◀──one batch── 4   rank + cut, delta-encode snapshot (postcard) │
            │  send_datagram per player        5 roleplay effects (serial, ordered) ──▶ writer ──┼──▶ RocksDB
            │                                  6 scripts (Luau, deadline per call)               │    (group commit)
            └────────────────────────────────────────────────────────────────────────────────────┘
```

The rules the target keeps:

- **The tick thread never blocks on the network and never allocates per datagram.** Datagrams are written into
  buffers kept across ticks and handed to the gateway once per tick, not once per datagram.
- **Per-player work is parallel and writes only that player.** Anything that touches two players (a transfer, a
  hit) is collected and applied in one serial, ordered phase, so the world after a tick does not depend on the
  thread count (SRV-4 showed this bit for bit).
- **Entities live in dense, recycled slots with a generation.** Every per-entity array (states, visible-set bits,
  baselines) is indexed by slot. A slot's generation changes on reuse, and a viewer that sees a new generation
  gets "left" and "entered", never a silent swap (IM-2's precondition).
- **Everything a client sends is bounded before it is used**: one latest state per player per tick, validated for
  speed, teleport and map bounds; events checked against a schema and a rate limit; connections capped.
- **Overload sheds load instead of falling behind.** A slow client loses snapshots, not the server's tick; a tick
  that overruns is counted and reported, and the per-player byte budget cuts the visible set before it grows the
  tick.
- **Value-bearing state goes through one writer** (ADR 0003) and is never written from the parallel phase.

## Order of work

Each step is one task and one pull request under 400 changed lines where possible, measured on the bots before
and after with [`research/load-test-bots/run.py`](research/load-test-bots/run.py) (2,000 and 3,000 bots in the
1.5 km square, the case that fails today). A step that does not move the tick or bytes by the amount expected
says so with the numbers, and the order is revisited.

### Phase 1: make the tick fit (one machine, 3,000 bots in a crowd)

| ID | Task | Expected effect | Depends on |
|---|---|---|---|
| SRV-6 (#94) | jemalloc as the global allocator of `sawgrass-server` and `sawgrass-bots` | Crowd of 2,000: p99 52 to about 26 ms (BOT-3 with `LD_PRELOAD`) | — |
| NET-9 (#95) | Snapshot datagrams written into buffers reused across ticks and handed to the gateway once per tick | Removes most of the 22% spent waking the gateway and the per-datagram allocations | — |
| IM-4 (#96) | Dense entity slots with generations; states in a slot-indexed array instead of hash maps | Removes the 8% of hash-map copying; precondition for IM-5 | — |
| IM-5 (#97) | Dense 50 m grid and bitset visible sets in `interest.rs`, as IM-2 measured them | Visible sets 1.8 to 2.8 times faster on the bots' movement (IM-3) | IM-4 |
| SRV-7 (#98) | Visible sets and snapshot encoding per player in parallel on rayon | Crowd tick p50 2.7 to 3.2 times lower on 4 threads (SRV-4) | NET-9, IM-5 |

Gate: 3,000 bots in the 1.5 km square with no dropped bots and tick p99 under 70% of the interval, on the
same 4-core machine as BOT-3.

Result so far (2026-11-25, [two-machine load](research/two-machine-load/README.md)): not counted as met. On a
dedicated 4-core machine, server on two of its cores, 3,800 bots ran at tick p99 23.8 and 24.4 ms with no overrun
and no dropped bot; on the BOT-3 VM, 3,000 bots ran at p99 37 to 38 ms. Whether the gate names the dedicated machine
is the maintainer's decision.

### Phase 2: fewer bytes, then 30 Hz

| ID | Task | Expected effect | Depends on |
|---|---|---|---|
| NET-10 | postcard for every payload; `PROTOCOL_VERSION` bump; bots follow | Snapshots 17% smaller on the bots' traffic (NET-8) | — |
| NET-11 | Delta snapshots against the last acknowledged one; client states carry the acknowledgement | Snapshot bytes cut by about 76% on the bots' traffic, one datagram per snapshot (NET-8) | NET-10, IM-4 |
| IM-6 | Rank the visible set and cut it to the per-player byte budget (issue #18) | Bounds per-player work and bytes in any crowd | IM-5 |
| SRV-8 | 30 Hz by default | The rate SRV-4 planned for | SRV-7, NET-11 |

Gate: 4,096 bots with the bots on a second machine, as the
[bots README](../crates/sawgrass-bots/README.md#bots-and-server-on-separate-machines) describes. This is stage
1's gate in the roadmap.

Result so far (2026-11-25, [two-machine load](research/two-machine-load/README.md)): not met. At 20 Hz, with 3,700
of the 4,096 bots on the server's machine, 4,096 bots ran with no overrun. On two server cores the gateway is the limit: with one gateway thread, tick p99
28 to 30 ms and 7 to 12% of snapshots dropped; with two, every snapshot delivered and p99 38 to 39 ms.

### Phase 3: reliability of the realtime core

| ID | Task |
|---|---|
| SRV-9 | Movement validation from architecture.md's Authority: speed, teleport, map bounds; counted per player for admins |
| NET-12 | Per-connection limits: client state rate, event schema and rate limit, handshake and connection caps under a flood |
| SRV-10 | Overload policy: a full send queue drops that player's oldest snapshots; repeated overruns are reported with their phase; a panicking system is reported and the server stops cleanly instead of continuing in an unknown state |
| BOT-4 | Bots that misbehave (floods, teleports, malformed packets, reconnect storms) as a load-test scenario, so phase 3 is tested on the bots |

### Phase 4: roleplay base (roadmap stage 2)

| ID | Task |
|---|---|
| DB-5 | The single writer and its RocksDB store with group commit, behind the store trait (ADR 0003); bots that move money as load |
| DB-6 | WAL copy to a second machine before acknowledgement, and the restore procedure (ADR 0003) |
| DB-7 | PostgreSQL archive fed from the operation records; `sawgrass-db`'s ledger becomes the archive's schema |
| SCRIPT-3 | Luau runtime in the server: resources, events, deadlines and memory caps (ADR 0005) |
| NET-13 | Voice relay over the same QUIC connection, Opus frames routed by the visible sets (NET-7) |

Phase 4 waits for phase 1's gate, so the tick it adds work to has room for it. Scaling step 2 of
architecture.md (gateways in their own processes) is not planned here: the gateway used about one core at 3,000
players in BOT-3, so it is started only if the 4,096 gate shows the gateway, not the tick, as the limit.

### Phase 5: sync before the game

The server and protocol side of roadmap step 8, as [ADR 0006](adr/0006-entity-sync-before-the-game.md) designs it.
None of it needs the game; every task is tested on the bots. The protocol tasks change what the client's network
core decodes, so they land in this order.

| ID | Task | Depends on |
|---|---|---|
| NET-15 | Entity kinds and model ids in added entities, and the `blocks` list in delta snapshots; `PROTOCOL_VERSION` 4 | ADR 0006 |
| NET-16 | The ped block: motion, flags, weapon, aim, seat, action layers, health and armour | NET-15 |
| NET-17 | The vehicle block, and owned-entity states with their epoch in the client state | NET-15 |
| SRV-13 | Vehicles, NPCs and objects as server entities that scripts and admin commands spawn and remove | NET-15 |
| SRV-14 | Owners for vehicles and NPCs: the four rules, epochs, hand-off and the checks on owned states | SRV-13, NET-17 |
| SRV-15 | Seats: enter and leave requests, decided by the server | SRV-14, NET-16 |
| SRV-16 | Hit reports checked against the snapshot history; health, armour and death on the server | NET-16 |
| SRV-17 | World state: time, weather, doors and routing buckets, sent on join and on change | NET-15 |
| BOT-5 | Bots that drive, change seats and owners and shoot, as a load-test scenario with bytes per player | SRV-14, SRV-15, SRV-16 |

Gate: the BOT-5 scenario at 3,000 bots in the 1.5 km square, with phase 1's tick budget, no refused state from a
well-behaved bot, and the bytes per player reported against architecture.md's budget.

## What this plan does not change

- The decisions themselves. A step that finds a chosen option does not work on the real server stops and proposes
  an ADR, as AGENTS.md asks.
- architecture.md. Its wire format section (DOC-1, #13) is written when NET-10 and NET-11 land; its tick rate
  already allows 30 Hz.
