# ADR 0006: Sync of peds, vehicles, damage and world state, built before the game

Status: proposed, 2026-11-25

## Context

Step 8 of the [roadmap](../roadmap.md#8-sync-in-the-game) lists what can be built before the game runs on PC: the
replicated state of peds and their animations and its encoding, the ownership rules and checks for vehicles and
NPCs, the server's checks of weapons and damage, and the server side of world state. Today the protocol carries one
kind of entity, a position, velocity and rotation with an id ([`sawgrass-protocol`](../../crates/sawgrass-protocol/src/lib.rs)),
and a client sends only its own state. The server validates that state (SRV-9), caps connections and events
(NET-12) and sends delta snapshots of the nearest 128 entities (NET-11, NET-14, IM-6).

The game has not been taken apart yet: nobody knows its animation clips, ped tasks, model names, weapons or doors,
or how it identifies them. The format must therefore be finished and tested on the bots now, without naming any of
those, so that the client module only has to read the game and fill in messages that already exist.

[architecture.md](../architecture.md#authority) already fixes who decides what: physics runs on clients, the server
assigns an owner to every vehicle and NPC and checks what the owner sends, and the server alone decides anything
with roleplay value. This ADR keeps that and decides how it is carried on the wire and checked.

## Decision

### 1. What is replicated, and how

- **Every entity has a kind**, fixed for the life of its id: player ped, NPC ped, vehicle or object. A slot reused
  for another kind gets a new generation, as now (IM-4).
- **Continuous state stays in the kinematics** that snapshots already carry: position, velocity, rotation, delta
  encoded as today.
- **Discrete state goes in per-kind blocks** carried by the same delta snapshot (section 6). A block is a short
  record of small fields with its own change mask, so a field that did not change costs nothing. Because snapshots
  are encoded against the last acknowledged one, a change in a block reaches the client even through lost
  datagrams, with no separate reliable message.
- **One-shot happenings are events**: a shot, a hit report, a request to enter a vehicle, a change of owner. They
  cannot be rebuilt from a later state, so they go on the reliable stream or, for effects nobody must miss but
  nobody needs late (a shot's muzzle flash), as datagrams relayed to viewers.

### 2. Game identifiers are opaque

The protocol never names a game asset. Models, animation clips, ped tasks, weapons, weather types and doors are an
`AssetId`, an opaque `u32` that only the client module interprets.

- *Assumption A1:* the game names its assets by a 32-bit hash of the name, as earlier games on this engine did. If
  so, the client module uses the game's value as is. If not, it keeps a per-build table from `AssetId` to the
  game's handle, like the function table of step 7. The wire format is the same either way.
- `0` means "none" (unarmed, no action) in every field.
- The server compares `AssetId`s but never interprets them. Lists of what is allowed (which models a player may
  spawn, which weapon a character holds, a weapon's damage and fire rate) come from resources and server
  configuration, written by server owners, never from game files in this repository.
- Appearance (a vehicle's colours and mods, a ped's clothes, tattoos and face) is an opaque byte string of at most
  256 bytes per entity (a placeholder, revisited when the game's appearance data is known), in a layout the client
  module defines. It is sent whole when an entity is added and when it changes, which is rare, never per tick. The
  server stores and relays it; scripts set it.

### 3. Peds and animation: intent, not clips

A remote ped is shown by the receiving client's own game, from a compact state of what the ped is doing. The
server sends no bones, clips or animation phases. The client replays the intent and lets its game animate it; the
result differs in small details between clients, as it does in other GTA multiplayer platforms.

The ped block:

| Field | Encoding | Meaning |
|---|---|---|
| `motion` | `u8` | Idle, walk, run, sprint, jump, fall, swim, climb, ragdoll, cover, in vehicle, dead, and values reserved for what the game turns out to need. An unknown value is shown as idle. |
| `flags` | `u8` bits | Crouched, aiming, firing, reloading, and reserved bits |
| `weapon` | `AssetId` | The weapon in hand, `0` unarmed |
| `aim` | two `u8` | Yaw relative to the body and pitch, about 1.4° and 0.7° steps; sent while aiming |
| `seat` | vehicle id and `u8` seat | Set while `motion` is "in vehicle". The ped's kinematics are then not updated: the client places it by its seat, so a passenger costs no bytes per tick |
| `action` | up to two layers: `AssetId`, start tick, `u8` flags | A full-body and an upper-body action (an emote, a scripted scenario, an interaction). The start tick lets a client that sees the ped late start the action at the right phase; flags say loop and whether it may be interrupted |
| `health`, `armour` | `u8` each | Set by the server only (section 5) |

- *Assumption A2:* the game can be told to perform an action and to move in a given way, and its animation
  follows. If some state has no clean way to be replayed, it gets a reserved `motion` value or an action layer,
  not a new mechanism.
- The client sends its own ped block with its own kinematics. The server takes `motion`, `flags`, `aim` and the
  action layers as the client says (they are presentation), checks `weapon` against what the character holds, and
  ignores `seat`, `health` and `armour` from clients: those are the server's.

### 4. Vehicles and NPCs: one owner each, with an epoch

The **owner** of a vehicle or NPC is the one client that simulates it and sends its state. The server picks the
owner, by these rules, in this order:

1. A vehicle with a driver is owned by the driver's client.
2. Otherwise the current owner keeps it while it is connected, in the same routing bucket and within the keep
   radius (placeholder 150 m).
3. Otherwise the nearest player within the grant radius (placeholder 100 m) who owns fewer than the cap
   (placeholder 24 entities) gets it.
4. Otherwise nobody owns it: the server keeps its last state with zero velocity, and every client shows it at
   rest.

Each change of owner increments the entity's **ownership epoch** (`u8`, wrapping). The server tells the old and the
new owner in one reliable event with the entity id and the new epoch. The new owner starts its local simulation from
the latest state it has from snapshots, with that state's velocity, so a car handed over at speed keeps moving.
Every owned-entity state a client sends carries the epoch; a state with another epoch, or from a client that does
not own the entity, is dropped and counted. Rule 2 and its hysteresis keep hand-offs rare, and rule 1 means a
driven car changes owner only when a driver gets in.

The server checks every owned-entity state as it checks a player's (SRV-9): map bounds, speed against a limit for
the entity's class, distance covered since the last accepted state. On a hand-off the check starts from the last
state the server accepted, so the new owner is not refused for the distance covered during the hand-off. The owner
must stay within the keep radius of what it owns.

**Seats** belong to the server. A client asks to enter a seat; the server grants it if the ped is close enough, the
seat is free, the vehicle is not locked and the scripts allow it, and then sets the ped's `seat`. Leaving works the
same way, and a ped that the owner's physics throws out is taken out of its seat when its client says so.

The vehicle block:

| Field | Encoding | Meaning |
|---|---|---|
| `controls` | `i8` steer, `i8` throttle, `u8` brake | What the owner's driver does, so other clients turn wheels and brake lights and extrapolate better |
| `angular velocity` | three `i16`, 0.01 rad/s | For extrapolation between snapshots |
| `flags` | `u16` bits | Engine, lights, high beam, siren, horn, indicators, and open doors as one bit per door |
| `damage` | `u8` engine, `u8` body, `u8` burst tyres as bits | Coarse damage. Deformation is not replicated (assumption A3: it cannot be reproduced exactly on other clients, so each client deforms on its own collisions) |

An NPC uses the ped block. Its owner runs its AI; the server and scripts decide that it exists and send its tasks as
events to the owner.

### 5. Damage: the client reports, the server decides

A ped's health and armour live on the server. A client cannot heal and cannot change another ped's health.

- The shooter's client sends **hit reports**: target, weapon, body zone (head, torso, arm, leg, other), the hit
  point, the shooter's position and the snapshot tick the shooter saw the target at. The hits of one client tick go
  in one event (up to 16, so a shotgun's pellets are one event), and combat events have their own rate limit, sized
  from the fastest weapon's fire rate, separate from NET-12's general event limit, so firing a fast weapon never
  closes the connection. The per-weapon fire-rate check is what bounds the hits.
- The server accepts it only if: the shooter is alive and holds that weapon; the weapon's fire rate allows another
  shot; the target is in the same routing bucket and in the shooter's visible set; the distance is within the
  weapon's range; the reported tick is at most the rewind limit old (placeholder 250 ms); and the target's position
  at that tick, from the server's snapshot history, is within a tolerance of the hit point. The server keeps that
  history already for delta snapshots.
- The server computes the damage from the weapon's figures and the zone, applies armour, and sets health in the
  ped block. At zero health it sets `motion` to dead and tells the scripts, which decide what happens next.
- A client may report damage to its own ped (a fall, a crash) and to what it owns; the server accepts such reports
  only as decreases, under a per-second cap.
- Shots are relayed to viewers near the shooter, as effects, and counted against the fire rate even when they hit
  nothing.
- *Limitation:* the server has no map geometry, so it cannot tell whether a wall was in the way. A line-of-sight
  check waits for collision data from the game, as the navmesh check does.

### 6. Wire format

Protocol version 4 changes the delta payload of [`delta`](../../crates/sawgrass-protocol/src/delta.rs) in two
places and leaves the rest as it is:

```text
baseline age in ticks
removed:  count, then id gaps                                         unchanged
changed:  count, then per entity: id gap, mask, kinematics            unchanged
added:    count, then per entity: id gap, kind, model AssetId,        kind and model are new
          kinematics, every block of its kind in full
blocks:   count, then per entity: id gap, length, block mask, fields  new list
```

The `blocks` list carries the entities whose block fields changed against the baseline, ascending by id, each field
as a varint or a difference from the baseline as the kinematics are. Appearance changes go in it too. The length
bounds each entry, so a decoder rejects a malformed entry without reading past it, and a later version that adds
fields at the end of a block needs only a small decoder change. It does not make versions compatible: until the
first release a client and a server speak only the identical `PROTOCOL_VERSION` (`is_compatible`). `MAX_ENTITIES` is derived again from the new worst-case
entity size.

From the client, the state datagram carries its own kinematics and ped block, then a count and that many owned
entities, each with its id, epoch, kinematics and block, cut to fit one datagram. Events (enter, leave, hit, shot,
owner changed, world state) are new codes in `ServerEvent` and `ClientEvent`, each checked against its schema and
rate limit as NET-12 requires.

### 7. World state

The server owns the time of day, the weather, doors and the instances players are in. A joining client gets all of
it in one event, then each change as an event.

- Time: the game minute, the rate in game minutes per real second, and the tick they hold at, so a client
  computes the clock itself between changes.
- Weather: current and next `AssetId` and the transition between them.
- Doors: an `AssetId` per door that a resource registered, with locked or unlocked and how far it is open. How the
  client finds the game's door for an id is part of the client module's table (assumption A4).
- Objects a server spawns are entities of kind object in snapshots. Static ones have no owner; one that can move is
  owned like a vehicle.
- Interiors and instances are routing buckets (architecture.md, Interest management). An entity's bucket is a
  field the server sets; entities in different buckets never see each other.

### 8. Order of work

The tasks are listed in [implementation-plan.md](../implementation-plan.md#phase-5-sync-before-the-game). Each
lands with tests and is checked on the bots, which drive cars, change seats and owners and shoot each other.

## Consequences

- The client module, when it comes, maps the game to these fields: read the local ped's motion and action and fill
  the ped block, play remote peds' blocks, simulate owned vehicles. Nothing in the protocol waits for the game.
- The protocol goes to version 4 once, in the first task that changes the delta payload. The client's network core
  (interpolation and prediction) reads the new lists; the kinematics it already decodes do not change.
- Every placeholder (radii, caps, the rewind limit, speed limits per class, the `motion` values the game needs) is a
  configuration value or a reserved code, revisited when the game runs. Assumptions A1 to A4 are checked in step 7;
  one that fails changes the client module's table, not the wire format.
- Physics between entities that different clients own (two cars colliding) is resolved by each owner separately.
  That is the accepted cost of client physics; the checks keep it from becoming a cheat.
- Line-of-sight and navmesh checks wait for the game's collision data.
- Intent-level animation means remote peds look approximately, not exactly, like on their own client. If a
  case turns out to need exact clips, it is an action layer with that clip's `AssetId`, not a change to this design.
