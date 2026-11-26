# Roadmap

Stages 0–2 do not need the game and run in parallel with the runtimes' work. Stage 3 starts when VIctory or XCL,
whichever comes first, runs the game to a world with a controllable character (ADR 0004).

Roleplay gameplay (characters, money, inventory, property, jobs) waits for the first two-client session: two
players on two PCs who join one server and see each other move. Until then the work is what that session and the
gameplay after it stand on: the server, the protocol, storage, the script runtime and the client's network core.

1. **Stage 0: repository foundation.** Workspace, rules, CI, claim bot, first tasks. Done by maintainers before
   the repository opens to the swarm.
2. **Stage 1: server without the game.** Protocol, gateway, simulation, interest management, bots, web map.
   Gates: 1000 bots on one machine, then 4096.
3. **Stage 2: roleplay foundations.** Roleplay state in server memory, stored in RocksDB, with a PostgreSQL archive
   (ADR 0003), the scripting runtime in Luau (ADR 0005), wallets in the server, the voice relay. No gameplay yet;
   everything tested with bots.
4. **Stage 3: client in the game.** A runtime adapter for the first runtime that gets there (VIctory or XCL), a
   module-loading API proposed upstream, hooks, sync of players, vehicles and damage, in-game UI. The other
   runtime's adapter follows. Runtime-independent client code (sync and prediction over the fake `GameApi`) can
   start earlier. Then the base resources (characters, money, inventory, property, jobs) and admin tools, written
   against the public scripting API once two clients see each other. Three steps below:
   [the client module](#7-client-module-in-the-game), [the roleplay base](#8-roleplay-base) and
   [sync in the game](#9-sync-in-the-game).
5. **Stage 4: launcher and releases.** Signing, auto-update, server list.
6. **Stage 5: scale.** Real players, zones only if measurements call for them.

The order in which the measured choices move into the server, with task IDs and gates, is in
[implementation-plan.md](implementation-plan.md).

## Steps in detail

The README's progress bar shows these eleven steps in order. Each one lists its work, what it depends on, its risks
and its difficulty. Difficulty estimates risk and unknowns, not size: 🟢 easy, 🟡 medium, 🟠 hard, 🔴 very hard.
"Before the game" says whether a piece of work can be built and tested on the bots and the fake `GameApi` now, or
only once the game runs on PC. Steps 3 to 6 and 9 have task IDs in [implementation-plan.md](implementation-plan.md); the
later steps get theirs when they are planned.

### 1. Foundation

🟢 Easy. Stage 0, done. Workspace, rules for people and agents, CI gates, the claim bot, the dispatcher and the review
agent.

### 2. Prototype and measurements

🟡 Medium. Stage 1, done. A first server, the protocol and the load-test bots, then a measured choice for each part of
the stack: storage, transport, serialization, compression, the visibility index, scripting, voice, the tick model and
the allocator, checked again on the real server under the bots (BOT-3). The results are in [research/](research/).

### 3. Make the tick fit

🟠 Hard. Stage 1, plan phase 1. jemalloc, one hand-off of datagrams per tick from reused buffers, dense entity slots
with generations, the dense 50 m grid with bitset visible sets, per-player work in parallel on rayon.

- Depends on: the measurements of step 2.
- Risks: parallel work must leave the world bit for bit as the serial tick does; the slot change touches every part
  of the server.
- Gate: 3,000 bots in the 1.5 km square on one machine, no dropped bots, tick p99 under 35 ms (70% of 20 Hz).

### 4. Fewer bytes, then 30 Hz

🟡 Medium. Stage 1, plan phase 2. Delta snapshots against the last acknowledged one, visible sets ranked and cut
to a byte budget per player, 30 Hz by default. postcard for every payload (NET-10) was dropped: the delta format
already writes its varints (#223).

- Depends on: step 3 (dense slots and the parallel tick).
- Risks: a delta against a baseline the client no longer has; a protocol version bump that every peer follows.
- Gate: 4,096 bots from a second machine, the stage 1 gate.

### 5. Reliability of the realtime core

🟡 Medium. Stage 1, plan phase 3. Movement validation (speed, teleport, map bounds), per-connection limits, an
overload policy that sheds load instead of falling behind, and bots that misbehave on purpose.

- Depends on: step 4 for the final message formats.
- Risks: abuse cases nobody thought of; the bots only find what they are told to try.

### 6. Roleplay foundations

🟠 Hard. Stage 2, plan phase 4. The single writer with RocksDB and group commit, its WAL copied to a second machine
before a change is acknowledged, the PostgreSQL archive, the Luau runtime with deadlines and memory caps, players'
wallets in the server, the voice relay. No roleplay gameplay: the resources that use all this are
[step 8](#8-roleplay-base).

- Depends on: step 3's gate, so the tick has room for the work this step adds.
- Risks: money created or lost on a crash or a failover; a script that hangs or exhausts the server; a scripting
  API that is hard to change once resources depend on it.

### 7. Client module in the game

🔴 Very hard. Stage 3, waits for the game. This is where the hardest part starts: the game is undocumented, it runs on
a young runtime, and every game or runtime update can move what the module relies on. VIctory first, XCL second ([ADR
0004](adr/0004-victory-client-runtime.md)).

| Work | Before the game |
|---|---|
| `Runtime` adapter for VIctory: load the module after the title starts, through a small module-loading and hook API proposed upstream, not a fork or an injector | Only the upstream proposal |
| Find game functions at run time per runtime and game build, and keep that table per build; no bytes from the executable in this repository | No |
| Hooks: the game's frame, creation and removal of peds, vehicles and objects, player input, camera, world streaming around remote players | No |
| Turn off single-player systems: story, ambient traffic and pedestrians, police and wanted level, saves; the server decides who and what exists | No |
| The real `GameApi` over the adapter; the fake one already serves the bots and tests | The trait and the fake: yes |
| Connect to the server, download resources, show errors in the game | The client connection code: yes |
| XCL adapter, after the licence question in ADR 0004 is settled | No |

- Depends on: VIctory or XCL running the game to a world with a controllable character; the upstream loading API.
- Gate: the first two-client session. Two players on two PCs join one server, see each other walk and run, and
  leave; the server's logs and counts show no refused state from either.

What the server needs for that session, so it is ready when the game runs:

| Need | Why |
|---|---|
| Map bounds, spawn point and movement limits as server settings, with today's values as defaults | The game's coordinates and speeds are not known; a wrong bound or limit refuses every real state |
| A recording of each connection's client states and events, replayable through the bots | The first real movement tunes interpolation, correction and the movement checks, and turns a bug into a test |
| A guide to run a server for a playtest: certificate, address, port, firewall, the flags to use | The first session is run by people who did not write the server |
| A live view of who is connected, where and what was refused | Shows at once whether two clients reached each other and why not |

### 8. Roleplay base

🟡 Medium. Stage 3, starts after step 7's gate. Characters, money, inventory, property, jobs, and admin tools as
resources in `resources/`, over the public scripting API and the foundations of step 6. They are built once two
clients see each other, so they are tested in the game and not only on the bots.

- Depends on: step 6; step 7's gate.
- Risks: a scripting API that is hard to change once resources depend on it; money that a resource can create.
- Risks: every game patch can move functions, so finding them must be automated and checked per build; a runtime
  that changes faster than the adapter; the licence question for XCL. Nothing here works around protection or keys:
  the module only runs inside a runtime the player already uses.

### 9. Sync in the game

🔴 Very hard. Stage 3. The server side and the client's network core are plan phase 5 and are built now; the rest
waits for the game. Making many players look right in one world, on physics and animation the
server does not own ([architecture.md, Authority](architecture.md#authority)).

| Work | Before the game |
|---|---|
| Remote players: interpolation between snapshots, prediction of the local player and correction from the server | Yes, over the fake `GameApi` with the bots |
| Animations and ped tasks (walking, running, entering vehicles, weapons, emotes, ragdoll): a compact state the game can replay; the largest piece of work | The replicated state and its encoding: yes; mapping it to the game: no |
| Vehicles: an owner simulates each one and the server checks it; seats and passengers, entering and leaving, damage, hand-off of ownership | The ownership rules and checks on the server: yes |
| Weapons and damage: the client reports a hit, the server decides it | The server's checks: yes |
| World state: time, weather, doors, objects, interiors, owned by the server | The server side: yes |
| NPCs a server spawns, simulated by an owner like vehicles | The server side: yes |
| In-game UI and chat, client Luau scripts in the sandbox, voice capture and playback by position | Script runtime and voice relay: yes (step 6); the game side: no |

- Depends on: step 7.
- Risks: game physics that differ between clients; animation states with no clean way to replicate; ownership
  hand-off at speed; cheating, since the client is open source and nothing on it can be trusted.
- Gate: players see each other move, drive and fight in one world, at the planned rate and budget.

### 10. Launcher and releases

🟡 Medium. Stage 4. A launcher (Rust and Tauri) that detects the player's runtime, checks its build against that
runtime's hash manifest, installs the signed client module, keeps it updated, lists servers and downloads their
resources; a release process with signing keys outside the repository.

- Depends on: step 7 for something to launch; the server list can start earlier.
- Risks: a security boundary on every player's machine, so signing and updates must not become a way to ship
  someone else's code. It never downloads the game or a runtime's converted data.

### 11. Scale

🔴 Very hard. Stage 5. Real players on public servers. Gateways in their own processes if the 4,096 gate shows them as
the limit; zones with entity hand-off only if measurements call for them; operations, monitoring, moderation and
admin tools.

- Depends on: steps 9 and 10.
- Risks: real players behave unlike bots; zones are the most error-prone step of the architecture
  ([Scaling path](architecture.md#scaling-path)).

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

- Where the community talks (Discord or GitHub only).
