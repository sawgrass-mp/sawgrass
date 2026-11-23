# Storage for roleplay state: in-process log, SpacetimeDB, PostgreSQL

Measured 2026-11-23 with SpacetimeDB 2.16.1, PostgreSQL 16.15 and a prototype in-process ledger. Task: DB-2.

The question: what is the fastest sound way to store roleplay state with money in it (money, inventory, property,
factions)? Keeping existing code is not a goal. Money must never be created or lost, and an acknowledged change
must survive a crash. This page records what was measured, what was not, and what follows from it.

## Short answer

| Design (same ledger, same rules, 64 callers) | Transfers per second, durable | One hot wallet | Restart with 1.8 M operations |
|---|---:|---:|---:|
| **In-process ledger**, log on disk, group commit | **72,781** | **71,969** | **1.7 s** replay |
| SpacetimeDB 2.16.1 | 11,352 | 10,707 | 13 s clean, 27 s after a crash |
| PostgreSQL, one SQL function per transfer | 5,666 | 917 | 0.8 s after a crash |
| PostgreSQL, current `sawgrass-db` code | 3,437 | 642 | (same) |

- **The fastest design is our own server holding the state in memory**, with an append-only log flushed to disk
  before each reply. Callers' transfers are flushed to disk together, one flush per batch. It was 6.4 times faster
  than SpacetimeDB and 13 to 21 times faster than PostgreSQL, with sub-millisecond median latency and no penalty
  for a hot wallet. A crash test (kill -9 under load) lost no acknowledged transfer, and the invariants held.
- **SpacetimeDB is second.** It works on the same principle (state in memory, logic next to it, batched durability)
  but adds a WebSocket hop and a WASM call per transfer. On this machine it was 2 to 3.3 times faster than
  PostgreSQL on even load and 12 to 17 times faster on a hot wallet.
- **PostgreSQL is last on writes**, mostly because every transfer holds row locks across a network round trip and a
  disk flush. It is still the best tool for history, queries, backups and replicas.
- **Proposed design:** authoritative roleplay state in the world server's memory, made durable by its own log with
  group commit, plus periodic snapshots. PostgreSQL becomes an asynchronous archive fed from the log, for queries,
  admin tools and history. That needs an ADR before code; see [Proposed design](#proposed-design) for what it
  costs.

The prototype is about 430 lines and passed one crash test. It is not a hardened storage engine, and the figures
come from one 4-core VM. Read [What was not measured](#what-was-not-measured) before relying on them.

## What was compared

The DB-1 money ledger, implemented with the same rules in each design: an idempotency key per operation, funds and
overflow checks, character wallets never negative, system wallets (treasury, shop) may go negative, and every
operation stored as a pair of entries with `balance_after`.

- **PostgreSQL, current code:** `sawgrass_db::transfer` over
  [`migrations/0001`](../../../migrations/0001_characters_wallets_ledger.sql). Five statements per transfer from
  the client. Triggers apply each entry, refuse any other balance change and check at commit that the entries sum
  to zero. Driver: [`transfer_load`](../../../crates/sawgrass-db/examples/transfer_load.rs).
- **PostgreSQL, one function:** the same transfer as one PL/pgSQL call
  ([`postgres/transfer_fn.sql`](postgres/transfer_fn.sql)), driven by `pgbench`.
- **SpacetimeDB:** a Rust module ([`stdb-module/`](stdb-module/src/lib.rs)) with one reducer per transfer, called
  from N WebSocket connections by [`stdb-client/`](stdb-client/src/main.rs). Its `check` reducer confirmed the
  invariants after 1.7 million transfers.
- **In-process ledger:** [`inproc/`](inproc/src/main.rs), standard library only. One writer thread owns every
  wallet; callers in the same process send a request over a channel and wait for the reply. The writer checks and
  applies the transfer and appends one record per operation (length, CRC32, both entries) to a log file. It replies
  once the record is on disk. Flush modes: `each` (one `fdatasync` per transfer), `group` (one per batch of up to
  4,096 waiting requests), `none` (no flush; a crash loses recent transfers). Recovery replays the log, drops a
  torn last record and checks the invariants.

Workload: 10,000 characters with funded bank wallets. `uniform` moves 1 to 100 units between two random characters;
`hot` has every transfer pay the one shop wallet. Each caller sends a transfer, waits for the reply and sends the
next. Runs last 20 seconds (10 seconds for the in-process ledger).

### Durability, stated for each

| Design | When the caller gets "done" | Lost on a crash |
|---|---|---|
| PostgreSQL, `synchronous_commit = on` | After the WAL record is flushed | Nothing acknowledged |
| SpacetimeDB, confirmed reads (default) | After the transaction is durable | Nothing acknowledged |
| In-process, `each` or `group` | After `fdatasync` of the log | Nothing acknowledged |
| In-process `none`, PostgreSQL `synchronous_commit = off`, SpacetimeDB confirmed reads off | Before the flush | Recent acknowledged transfers, always whole ones |

The headline figures use the first three rows only.

## Results

Machine: one cloud VM, 4 vCPU (Intel Xeon 2.1 GHz), 16 GB RAM, virtual disk (`open_datasync` about 8,500 flushes
per second). Database, server and callers all ran on this one machine. Single runs: treat differences under about
10% as noise.

Transfers per second (p99 latency):

| Pattern | Callers | PostgreSQL today | PostgreSQL function | SpacetimeDB | In-process, `each` | In-process, `group` |
|---|---:|---:|---:|---:|---:|---:|
| uniform | 1 | 660 (3.2 ms) | 1,143 | 871 (2.5 ms) | 5,546 (0.41 ms) | 5,444 (0.42 ms) |
| uniform | 16 | 3,506 (9.0 ms) | 4,852 | 7,135 (4.7 ms) | 6,116 (5.0 ms) | 44,718 (0.78 ms) |
| uniform | 64 | 3,437 (37 ms) | 5,666 | 11,352 (11 ms) | 6,128 (16 ms) | 72,781 (2.0 ms) |
| uniform | 256 | 2,697 (189 ms) | 4,487 | 12,438 (32 ms) | 6,425 (52 ms) | 93,862 (4.0 ms) |
| hot | 16 | 731 (114 ms) | 1,151 | 7,449 (5.1 ms) | 6,223 (4.8 ms) | 44,727 (0.74 ms) |
| hot | 64 | 642 (492 ms) | 917 | 10,707 (12 ms) | 6,187 (17 ms) | 71,969 (1.7 ms) |
| hot | 256 | not run | not run | not run | 6,308 (59 ms) | 97,107 (4.1 ms) |

Without durability (for scale, not for money):

| Variant | Result |
|---|---|
| In-process `none`, 64 callers | 110,806 tps, p99 0.84 ms (uniform); 108,181 (hot) |
| SpacetimeDB, confirmed reads off, 64 callers | 12,451 tps |
| SpacetimeDB, 64 connections with 16 calls in flight each (durable) | 21,822 tps, p99 113 ms |
| PostgreSQL function, `synchronous_commit = off`, 16 callers | 8,417 tps uniform, 2,196 hot |

Restart after a crash or shutdown, with about 1.8 million transfers stored:

| Design | Time until it answers | Notes |
|---|---:|---|
| In-process | 1.7 s | Replays the whole 151 MB log (82 bytes per operation) after dropping the page cache; grows with the log until snapshots exist |
| SpacetimeDB | 27 s after a crash, 13 s after a clean stop | Loads all tables into memory; an empty server starts in 0.15 s |
| PostgreSQL | 0.8 s after a crash | Redoes only the WAL since the last checkpoint; data stays on disk |

Crash test of the in-process ledger: killed with SIGKILL after 6 seconds of `group` load at 64 callers. Callers had
received 426,199 acknowledgements (counted every 50 ms). The log replayed 426,706 transfers plus the 10,000 seed
operations, with no torn record and all invariants holding. A process kill leaves the OS page cache intact, so this
checks the recovery logic, not a power cut; `fdatasync` before the reply is what covers a power cut.

### What the numbers mean

- **Network hops and locks dominate.** PostgreSQL spends most of a transfer on round trips and on row locks held
  until the commit is flushed, so one hot row caps it below about 1,000 per second. Some of its cost is our own
  protective triggers, and a plain table would be faster than measured, but that would not change the order.
- **Group commit is what makes the in-process design fast.** Flushing each transfer alone gave about 6,000 per
  second, the speed of the disk. One flush per batch gave 45,000 to 97,000 with lower latency at the same time.
  SpacetimeDB and PostgreSQL also batch flushes, but they pay for the network, protocol and (for SpacetimeDB) a
  WASM call on every transfer.
- **A hot wallet costs nothing in memory.** One writer applies transfers one after another, so there is no lock to
  wait for. SpacetimeDB behaves the same; PostgreSQL does not.
- **The in-process figure is a lower bound for the design.** Callers here are threads that pass messages, one
  transfer at a time. In the real server the tick could hand over a whole tick's transfers at once.
- **Our expected load** is a guess until BOT-2: about 1,200 storage writes per second at peak for 4096 players. All
  three designs cover that on even load; PostgreSQL does not cover it on one hot wallet.

### What was not measured

- Machines larger than 4 cores, callers on another machine, runs longer than 30 seconds, and a real power cut.
- Reads under write load, subscriptions (SpacetimeDB's push of changes to clients), and memory use of the
  in-process ledger. It keeps every idempotency key in memory as built; a real one keeps a bounded window.
- Inventory, property and factions. The ledger is the strictest of them; that they behave alike is an assumption.
- The asynchronous PostgreSQL archive of the proposed design, and snapshots for faster restarts. Neither exists
  yet.

## Proposed design

Authoritative roleplay state lives in the world server's memory, in one writer per kind of state (or per shard of
wallets). Changes are made durable by an append-only log with group commit before the caller is told "done".
Snapshots bound the replay time. PostgreSQL is fed from the log asynchronously and serves history, admin queries,
backups and analytics, but never decides a balance.

What it costs, in honesty:

- **We own a storage engine.** Log format, CRC, torn-tail recovery, snapshots, log rotation and checks against
  silent corruption all become our code and need tests: crash tests at random points, fuzzed logs, replay
  equivalence against PostgreSQL.
- **One machine holds the truth.** A dead disk loses everything not yet copied elsewhere. Shipping the log to a
  second machine (and to the PostgreSQL archive) has to come with it, ideally before acknowledgement for money.
- **Reads must respect durability.** With group commit, a transfer is applied in memory before its flush. Game
  logic that reads a balance in that window sees money that a crash could still undo. Either reads go through the
  writer after the flush, or the server accepts that, as SpacetimeDB does with confirmed reads off.
- **The architecture changes.** `docs/architecture.md` names PostgreSQL as the source of truth for characters,
  money, inventory and property. Moving that is an ADR first, then the code.

SpacetimeDB would give most of this ready-made (in-memory state, log, snapshots, logic next to the data). It is
about 6 times slower here than our own process, needs a network hop from the world server, and comes with the
licence terms below.

## The realtime core

The core (QUIC datagrams through gateways, the spatial grid, the 20 Hz tick) is unchanged by any of this:

- **Transport.** SpacetimeDB clients connect over a WebSocket (plus an HTTP API). The docs mention no UDP, QUIC,
  WebTransport or unreliable messages. Over TCP one lost packet holds back everything after it, which is why
  movement in [architecture.md](../../architecture.md) uses unreliable datagrams.
- **Update model.** Subscriptions send at most one update per transaction per client. Our interest management sends
  each player a ranked snapshot per tick, cut to a byte budget, with hysteresis. Subscriptions offer no such
  budget.
- **Volume.** 4096 players at 20 Hz is 81,920 position updates per second before fan-out. The best SpacetimeDB
  figure here was about 22,000 reducer calls per second, on 4 cores.

## SpacetimeDB licence

Checked against the licence files of the 2.16.1 crates.

| Part | Licence |
|---|---|
| Server (`spacetimedb-standalone`) | Business Source License 1.1 |
| Module bindings (`spacetimedb`), `spacetimedb-sats`, `spacetimedb-lib`, `spacetimedb-primitives` | Apache-2.0 |
| Client SDK (`spacetimedb-sdk`), protocol messages (`spacetimedb-client-api-messages`) | Business Source License 1.1 |

The BSL's Additional Use Grant allows production use "with no more than one SpacetimeDB instance in production",
and not as a commercial "Database Service" where third parties create their own tables. Each version becomes "GNU
Affero General Public License v3.0 with a linking exception" four years after release (2.16.1: 2031-11-19).

**Without the BSL SDK.** A module can be written with the Apache-2.0 crates only. A client could be written from
the published WebSocket protocol, using the Apache-2.0 `spacetimedb-sats` for the BSATN encoding, without the SDK
or the BSL message crate. That keeps BSL code out of our binaries. The server itself is still BSL, and every
operator runs it under the grant. Whether an independent client is free of the BSL is a legal question, not
answered here.

**One instance in practice.** For one world server with one SpacetimeDB process, the limit is met. It rules out a
replica or hot standby, a second instance for another region or a zone split (stage 5 of the roadmap), and sharding
across instances. Whether each community that runs its own Sawgrass server counts as a separate "application or
service" is not clear from the text; a host running many servers for others may need Clockwork Labs to confirm, or
a commercial licence.

The project's own licence is being reconsidered separately. That affects whether the SDK may be linked, but not the
one-instance limit, which binds whoever runs the server.

## Reproduce

PostgreSQL, current code (an empty database; the example runs the migrations and seeds the wallets):

```sh
createdb sawgrass_load
cargo run -p sawgrass-db --release --example transfer_load -- postgres://postgres@localhost/sawgrass_load 64 20 uniform
```

PostgreSQL, one function call: after a run of the example, load
[`postgres/transfer_fn.sql`](postgres/transfer_fn.sql) into the same database, run `CREATE SEQUENCE pgb_seq`, then
`pgbench -n -M prepared -c 64 -j 4 -T 20 -f postgres/uniform.pgb sawgrass_load` (or `hot.pgb`). The scripts assume
bank wallets have ids 4, 6, ... 20002 and the shop is wallet 2, as the example creates them.

SpacetimeDB (with the `spacetime` CLI and a local server from <https://spacetimedb.com/install>):

```sh
cd stdb-module && cargo build --release --target wasm32-unknown-unknown && cd ..
spacetime publish -s local -b stdb-module/target/wasm32-unknown-unknown/release/ledger_module.wasm ledger
spacetime call -s local ledger seed 10000 100000000
spacetime generate --lang rust --out-dir stdb-client/src/module_bindings \
    --bin-path stdb-module/target/wasm32-unknown-unknown/release/ledger_module.wasm
cd stdb-client && cargo run --release -- ledger 64 20 uniform 1   # database, callers, seconds, pattern, in flight
spacetime call -s local ledger check                              # fails if the ledger is inconsistent
```

In-process ledger:

```sh
cd inproc
cargo run --release -- run /tmp/ledger.log 64 10 uniform group    # log, callers, seconds, pattern, each|group|none
cargo run --release -- recover /tmp/ledger.log                    # replay time and invariants
```

## Sources

- SpacetimeDB licences: the `LICENSE` files of the 2.16.1 crates on crates.io.
- Connection and transport: <https://spacetimedb.com/docs/clients/connection>, <https://spacetimedb.com/docs/ws>
- Subscription semantics: <https://spacetimedb.com/docs/clients/subscriptions/semantics>
- Self-hosting and storage: <https://spacetimedb.com/docs/how-to/deploy/self-hosting>,
  <https://spacetimedb.com/docs/tables/storage-and-cache-policies>
- Vendor benchmark and a critique of it: <https://spacetimedb.com/blog/benchmarking>,
  <https://dev.to/tanay/a-closer-look-at-the-spacetimedb-v2-benchmark-is-it-really-23x-faster-than-sqlite-2l9f>.
  Their workloads differ from ours. The figures above are our own.
