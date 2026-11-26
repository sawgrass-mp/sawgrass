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
 launcher ──starts──▶ game (VIctory or XCL) + client module ────┐
                                                                │ QUIC
 load-test bots (same protocol, no game) ──────────────────────┤
                                                                ▼
                                                   gateways ×3–4 ──events──▶ world simulation ──▶ RocksDB ··▶ PostgreSQL
                                                                                    │                   (archive)
                                                                                    └──▶ web map (admin)
```

- **Gateways** terminate player connections (QUIC through `quinn`: unreliable datagrams for movement, streams
  for events and resource downloads), enforce rate limits, and build each player's snapshot from the world state.
  They scale horizontally because outgoing traffic, not simulation, is the bottleneck.
- **World simulation** is one multithreaded process holding every entity in an ECS. It owns authority decisions,
  runs the game-mode scripts and decides who sees what.
- **Roleplay state** (characters, money, inventory, property) is owned by one writer in the world server, held in memory
  and stored in RocksDB with group commit ([ADR 0003](adr/0003-in-memory-roleplay-state.md)). A change is acknowledged
  only once it is on disk.
- **PostgreSQL** is the archive, fed from the stored operations asynchronously, for history, admin queries and backups.
  It never decides a balance.
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
use events (server ↔ client), RPC, replicated per-entity state, commands and permissions. Scripts are written in
Luau, embedded through `mlua` with native code generation ([ADR 0005](adr/0005-luau-scripting.md)). Every call into
a resource runs under a deadline enforced by Luau's interrupt callback, and every resource has a memory cap. Client
scripts are downloaded from servers and run on players' machines, so scripts on both sides run in Luau's sandbox
with no file, network or process access.

## Client and launcher

- The game runs on PC through one of two **runtimes** ([ADR 0004](adr/0004-victory-client-runtime.md)):
  [VIctory](https://github.com/Marvisak/VIctory), the furthest along, or [XCL](https://github.com/ivall/xcl). Both run
  the console build's own x86-64 code natively and reimplement the console libraries beneath it. In-game testing starts
  on whichever reaches a world with a controllable character first.
- The client module is a Rust library with a C ABI. A `Runtime` adapter per runtime loads it into the game, installs
  hooks and locates game functions; only the adapters know which runtime they serve. Functions are found at run time by
  pattern, per runtime and game version; no bytes from the executable are stored in this repository.
- All game access goes through a `GameApi` trait with two implementations: the real one over a runtime adapter,
  and a fake one used by bots and tests. Nothing above `GameApi` knows which runtime it runs on.
- The launcher (Rust and Tauri) detects the player's runtime, checks its build against that runtime's hash manifest,
  installs the signed client module, lists servers, downloads server resources and starts the game. It never downloads
  or unpacks the game.

## Anti-cheat

The client is open source and runs on an open runtime, so nothing on the client can be trusted. Protection
lives on the server: authority as above, event validation, movement checks, per-player statistics and an event
log for admins.

## Wire format

This section describes `PROTOCOL_VERSION` 14, as `crates/sawgrass-protocol` and `crates/sawgrass-gateway` encode
it. A pull request that bumps `PROTOCOL_VERSION` updates it. Until the first release a client and a server talk only
when their versions are identical.

### Connection

- QUIC through quinn, with ALPN `sawgrass` and TLS 1.3 only. The client verifies the server's certificate against
  the root certificates it was given and the server name it connects with.
- Transport limits: a client may open exactly one bidirectional stream and no unidirectional ones; the server opens
  none. The idle timeout is 30 seconds with a keep-alive every 5; stream and connection receive windows are 256 KiB.
- The client opens the stream and sends an 8-byte hello: the 4 bytes `SWGR`, then its protocol version (`u32`). The
  server answers with its own version (`u32`, 4 bytes) and the session starts. On a version it does not accept, the
  server closes the connection with application error code 1 and its own version (`u32`) as the 4-byte reason, so
  the client can tell which side is out of date; a client that does not accept the server's version closes the same
  way with its own. A hello without `SWGR` is closed with code 0 and the reason `bad hello`. The handshake must
  finish within 10 seconds.
- After the handshake the stream carries reliable events, each framed as a `u32` length and that many bytes, at
  most 65,536. A longer frame closes the connection with code 0 and the reason `event too long`.
- Everything else travels in unreliable QUIC datagrams of at most 1,100 bytes, header included. QUIC guarantees
  1,200-byte UDP payloads; the rest is left for QUIC's own overhead.
- Every stream frame and every datagram is one packet: a [header](#packet-header) and a payload. Events go only on
  the stream and the other kinds only in datagrams; a packet on the wrong channel is an error.

### Conventions

- Fixed-size integers are little-endian.
- A varint is LEB128, as postcard writes it: 7 bits per byte, lowest first, the high bit set on every byte but the
  last. It takes at most 10 bytes, the tenth holding only bit 63; a longer encoding than needed (`80 00` for 0) is
  accepted. A signed varint is zigzagged first: `(v << 1) ^ (v >> 63)`. A varint that carries a `u32` (an age, an
  id part, a model, a field, a rotation) must fit in 32 bits.
- A payload ends exactly where its encoding does. Bytes left over, an unknown kind or event code, a count above its
  limit or a value out of range reject the whole packet. Encoders refuse a value that is not finite or out of range
  instead of clamping it.
- What a rejected packet costs depends on where it came from: the server drops a client state that does not decode,
  ignores datagrams of other kinds from a client, and closes the connection on a client event that does not decode.
  Rate limits per connection apply on top of the format and can drop states or close the connection.
- An **entity id** is a `u32` the server gives: the slot index in the high 16 bits, the slot's generation in the
  low 16 bits. A reused slot gets a new generation, so a viewer sees the old entity leave and the new one enter.
- An **`AssetId`** is an opaque `u32` naming a game asset (model, weapon, action, weather, door) that only the
  client module interprets ([ADR 0006](adr/0006-entity-sync-before-the-game.md)). `0` means none.

### Packet header

7 bytes at the start of every packet:

| Bytes | Field | Meaning |
|---|---|---|
| 1 | kind (`u8`) | `1` whole snapshot, `2` client state, `3` event, `4` delta snapshot |
| 2 | sequence (`u16`) | A counter that wraps: in snapshots, one step per datagram, so per fragment; in client states, the client's own count of states. `0` in server events; the server does not read it in client events |
| 4 | tick (`u32`) | From the server, the tick the payload describes, or for an event the tick it was sent at. In a client state, the newest snapshot tick the client decoded completely, `0` for none: its acknowledgement. The server does not read it in client events |

### Quantized values

| Value | Wire | Range | Worst-case error |
|---|---|---|---|
| Position | 9 bytes: x, y, z as `i24` centimetres | ±83,886.07 m per axis | 0.9 cm per axis after rounding to `f32` |
| Velocity | 6 bytes: x, y, z as `i16` centimetres per second | ±327.67 m/s per axis | 0.51 cm/s per axis |
| Rotation | 4 bytes: `u32`, smallest three | any unit quaternion | 0.28° |

The ranges are what encoders accept; decoders take every value of the integer type, down to −8,388,608 cm and
−32,768 cm/s.

A rotation is a unit quaternion `[x, y, z, w]`. Bits 30–31 hold the index of its largest component. The other three
follow in index order, 10 bits each, at bits 20–29, 10–19 and 0–9. A code `c` decodes to `(c / 1023 · 2 − 1) / √2`.
The largest component is rebuilt as `√(max(0, 1 − sum of the squares of the other three))`, always positive, since
`q` and `−q` are the same rotation, and the four components are then divided by their length.

**Kinematics** are a position, a velocity and a rotation in that order, 19 bytes. An **entity state** is an entity
id (`u32`) followed by kinematics, 23 bytes.

### Client state (kind 2, datagram)

Sent by the client every client tick. The payload must fit one datagram, 1,093 bytes after the header.

1. The player's kinematics, 19 bytes.
2. Optional: the player's [ped block](#entity-blocks): a field count (varint, at most 16), then the fields
   (varints).
3. Optional, only after a ped block: the entities the client simulates. A count (varint, at most 16), then for each
   one its id (varint), its ownership epoch (`u8`), its kinematics (19 bytes), a field count (varint, at most 16)
   and its fields: a vehicle block for a vehicle, a ped block for an NPC.

A ped block with fewer than 14 fields rejects the whole state. The server takes only the presentation fields of the
player's ped block (motion, flags, weapon, aim and actions) and keeps its own vehicle, seat, health and armour; it
also keeps the motions "in vehicle" and "dead", which a client can neither enter nor leave. It drops an owned state
whose epoch is not the one its last ownership event gave. The acknowledgement it uses is the highest one it has seen,
so a reordered state cannot lower it.

### Whole snapshot (kind 1, datagram)

A count (`u8`, at most 47), then that many entity states; a larger snapshot is split over several datagrams with the
same tick. The server has sent delta snapshots instead since version 3; this format stays for tools and older
recordings.

### Delta snapshot (kind 4, datagram)

A snapshot is encoded against a **baseline**, an older snapshot the client acknowledged. After the header, whose tick
is the snapshot's, each datagram carries one fragment byte and a part of the payload of at most 1,092 bytes. The
fragment byte holds the fragment's index in its high nibble and the fragment count minus one in its low nibble, so a
payload spans at most 16 datagrams. Every fragment but the last is full; an empty payload is one fragment with the
byte `00`. A fragment whose index is not below its count, whose part is longer than 1,092 bytes, or whose count
differs from earlier fragments of its tick is an error. A client applies a tick only once every fragment of it has
arrived, and ignores fragments of a tick older than one it has started. The server sends at least one datagram every
tick, even for an empty snapshot.

The payload is made of varints, except the one-byte mask and kind:

```text
age      baseline age in ticks; 0: no baseline, every entity is in "added"
removed  count, then per entity: id
changed  count, then per entity: id, mask (one byte), the changed kinematics
added    count, then per entity: id, kinematics, kind (one byte), model, field count, fields
blocks   count, then per entity: id, length, field mask, the changed fields
```

- **Baseline.** The baseline is the snapshot of tick `tick − age` as the client decoded it, info included. The
  server encodes against the client's acknowledgement when it sent that client a snapshot at that tick and the tick
  is at most 15 ticks old, and sends age 0 otherwise. A client keeps one decoded snapshot per value of `tick mod 16`,
  the newest; one that no longer has the baseline a payload names drops that snapshot and waits for one against a
  newer acknowledgement.
- **Ids** ascend within each list. Each id is two varints: the gap between its high 16 bits and those of the previous
  id in the same list (the list's first id counts from 0), then its low 16 bits. Ids in `removed` must be in the
  baseline and ids in `changed` among the entities kept from it; an id in `added` must not be among them; an id in
  `blocks` must be in the resulting snapshot.
- **Counts.** Each list, and the resulting snapshot, holds at most 139 entities: as many as fit in 16 fragments
  whatever their values. A count larger than the bytes left is an error. The server sends at most 128.
- **Changed.** Mask bits 0–2 are the position's x, y and z, bits 3–5 the velocity's, bit 6 a rotation whose largest
  component is the same, bit 7 a rotation sent whole. A mask of 0, or with both bits 6 and 7, is invalid. The values
  follow in bit order: each position and velocity axis as a signed difference from the baseline, in centimetres or
  centimetres per second; for bit 6 the three 10-bit components as signed differences, bits 20–29 first, each
  result in 0 to 1023, and the largest component's index kept; for bit 7 the rotation's `u32` as a varint.
- **Added.** The position and velocity as signed varints (centimetres, centimetres per second), the rotation's `u32`
  as a varint, the kind (`1` player, `2` NPC, `3` vehicle, `4` object), the model (`AssetId`), a field count of at
  most 16 and the block fields. An id's kind, model and field count never change; the server gives a new generation
  instead.
- **Blocks.** An entry carries new values for an entity of the resulting snapshot. The length is the number of
  bytes after it in the entry. The mask is a varint, bit `i` for field `i`; it is not 0 and sets no bit past the
  entity's field count. The values of the set fields follow in field order, whole, not as differences. The server
  sends an entry for an entity kept from the baseline whose fields changed after the acknowledged tick, with every
  field set, so an entry may repeat values the client has. A decoder that needs only kinematics still checks each
  added entity's kind, model and fields, and skips block entries by their length.

### Entity blocks

A block is a list of at most 16 `u32` fields whose meaning depends on the entity's kind. The snapshot decoder checks
only that count; reading a block of a kind checks the rest. It reads the fields it knows and ignores any after them,
which a later version may add; fewer fields than it knows, or a field wider than its entry, is an error. A player
may be sent with no fields, before its client has sent a ped block. No block is defined for objects yet.

Ped block, for players and NPCs (14 fields):

| Field | Value |
|---|---|
| 0 | Motion (`u8`): `0` idle, `1` walk, `2` run, `3` sprint, `4` jump, `5` fall, `6` swim, `7` climb, `8` ragdoll, `9` cover, `10` in vehicle, `11` dead. Other values are reserved and shown as idle |
| 1 | Flags (`u8`): `1` crouched, `2` aiming, `4` firing, `8` reloading |
| 2 | Weapon in hand (`AssetId`), `0` unarmed |
| 3 | Aim: yaw relative to the body in the low byte (256 steps a turn), pitch in the next (`0` straight down, `128` level, `255` straight up) |
| 4 | The vehicle the ped sits in (entity id); meaningful only with a seat |
| 5 | Seat (`u8`): `0` none, otherwise the seat index plus one |
| 6, 7, 8 | Full-body action: `AssetId`, start tick, flags (`u8`: `1` loop, `2` interruptible) |
| 9, 10, 11 | Upper-body action, as fields 6 to 8 |
| 12, 13 | Health and armour (`u8` each), set by the server only |

Vehicle block (6 fields):

| Field | Value |
|---|---|
| 0 | Controls: steer (`i8`, `−127` full left) in the low byte, throttle (`i8`, reverse below 0) in the next, brake (`u8`) in the third |
| 1, 2, 3 | Angular velocity about x, y and z: an `i16` in 0.01 rad/s, as its 16 bits |
| 4 | Flags (`u16`): `1` engine, `2` lights, `4` high beam, `8` siren, `16` horn, `32` left indicator, `64` right indicator; bit `8 + n` is door `n` open, up to 8 doors |
| 5 | Damage: engine health (`u8`, `255` intact) in the low byte, body health in the next, burst tyres as bits in the third |

### Events (kind 3, stream)

After the header, a code byte and the event's fields, little-endian. Every event has a fixed size but a shot, whose
size follows its hit count. Codes below 128 go from the server to a client, codes from 128 from a client to the
server. An unknown code is an error.

From the server:

| Code | Event | Fields |
|---|---|---|
| 1 | Joined | The player's entity id (`u32`), the server's tick rate in Hz (`u16`) |
| 2 | Stats | Eight `u32`: tick p50, p99, maximum and interval in µs, overruns, players, process and tick-thread CPU in thousandths of a core (`u32::MAX` when unknown); then two `u64`: bytes sent and received per second. Only to load-test clients that asked, on servers that allow it |
| 3 | Paid | The payment's key (`u64`), the result (`u8`: `0` done, `1` insufficient funds, `2` unknown payee, `3` invalid, `4` failed, `5` refused), the payer's balance (`i64`) |
| 4 | Ownership | Entity id (`u32`), its new epoch (`u8`), whether this client simulates it now (`u8`, 0 or 1) |
| 5 | Corrected | The entity state the server holds after refusing one the client sent for that entity (23 bytes) |
| 6 | Environment | The tick it holds at (`u32`), the game minute of the day then (`u16`, below 1440), the clock's rate in hundredths of a game minute per real second (`u16`, 0 stops it), the weather and the next weather (`AssetId` each, the same when it is not changing), the transition between them (`u8`, 0 none to 255 done) |
| 7 | Door | Door id (`AssetId`), locked (`u8`, 0 or 1), how far open (`u8`, 0 shut to 255 wide) |
| 8 | Seat | Vehicle id (`u32`, `0` with "not seated"), seat index (`u8`, `0` the driver's), the answer (`u8`: `0` granted, `1` left, `2` not a vehicle, `3` no such seat, `4` taken, `5` locked, `6` cannot reach, `7` already seated, `8` not seated) |

From a client:

| Code | Event | Fields |
|---|---|---|
| 128 | Request stats | None |
| 129 | Pay | The payment's key (`u64`), the payee's entity id (`u32`), the amount (`u32`) |
| 130 | Enter seat | Vehicle id (`u32`), seat index (`u8`, `0` the driver's) |
| 131 | Leave seat | None |
| 132 | Shot | The snapshot tick the targets were drawn from (`u32`), how far the render time was past it toward the next tick (`u8`, out of 256), the weapon (`AssetId`), the shooter's position (9 bytes), the hit count (`u8`, at most 16), then per hit: target id (`u32`), body zone (`u8`: `0` head, `1` torso, `2` arm, `3` leg, `4` other, and every hit on a non-ped), hit point (9 bytes) |

The seat index in events counts from 0; in the ped block it counts from 1, with 0 meaning no seat.
