# SpacetimeDB versus PostgreSQL for roleplay state

Measured 2026-11-23 with SpacetimeDB 2.16.1 and PostgreSQL 16.15. Task: DB-2.

The question was whether [SpacetimeDB](https://spacetimedb.com) should replace PostgreSQL for the slow roleplay
state (money, inventory, property, factions), and whether it deserves a place in the realtime core. This page
records what was checked, what was measured and what was not, so the decision does not have to be argued again from
memory.

## Short answer

- **SpacetimeDB is faster, clearly.** On the same money ledger with the same checks it ran 3.3 times more transfers
  per second than our current PostgreSQL code with even load across wallets, and about 16 times more when every
  transfer hits one wallet. Latency was lower too.
- **We do not need that speed yet.** PostgreSQL handled about 3,500 transfers per second on a 4-core machine with
  every commit flushed to disk. Our rough worst case for 4096 players is a few hundred money operations per second.
- **The costs are in licensing and operations, not speed.** The server and the Rust client SDK are under the
  Business Source License: one production instance per application, no replicas, and the SDK does not pass our
  `deny.toml`. All data lives in RAM, and our ledger only grows.
- **Not for the realtime core.** Clients connect over WebSocket (TCP) only. Our core is built on QUIC datagrams,
  and the core's own load is above what SpacetimeDB reached here.
- **Decision proposed: keep PostgreSQL.** Revisit when the conditions under [When to revisit](#when-to-revisit) are
  met.

## What was compared

The DB-1 money ledger, implemented twice with the same rules:

- **PostgreSQL:** the real code, `sawgrass_db::transfer` over the schema in
  [`migrations/0001`](../../../migrations/0001_characters_wallets_ledger.sql). One transfer is a transaction of
  five statements from the client: insert the operation (idempotency key), lock both wallets, insert two ledger
  entries. Triggers apply each entry to its wallet, refuse any other balance change and check at commit that the
  entries sum to zero.
- **SpacetimeDB:** [`stdb-module/`](stdb-module/src/lib.rs), the same tables and the same checks (idempotency key,
  funds, overflow, append-only entries with `balance_after`) in a Rust reducer. A reducer is one transaction;
  returning an error rolls it back. Its `check` reducer confirmed afterwards that balances still summed to zero and
  matched the ledger after 1.7 million transfers.

Workload: 10,000 characters, each with a funded bank wallet. `uniform` moves 1 to 100 units between two random
characters. `hot` has every character pay the one shop wallet: one contended row, like a popular store or the
treasury paying salaries. Each client sends a transfer, waits for the reply and sends the next (one in flight),
unless the table says otherwise. Each row is a 20-second run.

Durability was equal: PostgreSQL ran with `synchronous_commit = on` (the reply waits for the WAL flush), and
SpacetimeDB with confirmed reads, its default since 2.0 (the reply waits until the transaction is durable).

## Results

Machine: one cloud VM, 4 vCPU (Intel Xeon 2.1 GHz), 16 GB RAM, virtual disk (`open_datasync` about 8,500 flushes
per second). The database and the load client ran on the same machine. Single runs, not averaged: treat differences
under about 10% as noise.

| Pattern | Clients | PostgreSQL, `transfer` today | PostgreSQL, one function call | SpacetimeDB |
|---|---:|---:|---:|---:|
| uniform | 1 | 660 tps, p99 3.2 ms | 1,143 tps | 871 tps, p99 2.5 ms |
| uniform | 16 | 3,506 tps, p99 9.0 ms | 4,852 tps | 7,135 tps, p99 4.7 ms |
| uniform | 64 | 3,437 tps, p99 37 ms | 5,666 tps | 11,352 tps, p99 11 ms |
| uniform | 256 | 2,697 tps, p99 189 ms | 4,487 tps | 12,438 tps, p99 32 ms |
| hot | 16 | 731 tps, p99 114 ms | 1,151 tps | 7,449 tps, p99 5.1 ms |
| hot | 64 | 642 tps, p99 492 ms | 917 tps | 10,707 tps, p99 12 ms |

More rows:

| Variant | Result |
|---|---|
| SpacetimeDB, 64 clients, 16 calls in flight each | 21,822 tps, p50 44 ms, p99 113 ms |
| SpacetimeDB, 64 clients, confirmed reads off | 12,451 tps, p99 9.2 ms (durability barely matters here) |
| PostgreSQL function, `synchronous_commit = off`, uniform, 16 clients | 8,417 tps |
| PostgreSQL function, `synchronous_commit = off`, hot, 16 clients | 2,196 tps |

"One function call" is the same transfer moved into a PL/pgSQL function
([`postgres/transfer_fn.sql`](postgres/transfer_fn.sql)) and driven by `pgbench`, so one round trip instead of
five. It uses a different client from the other PostgreSQL column, so it shows the size of the round-trip cost, not
an exact figure.

Resources: during the 64-client SpacetimeDB run the server used about 1.6 cores. Its reducer thread was at about
60%, so the limit was not reducer execution yet. Resident memory was 1.1 GB with 1.7 million operations and 3.4
million ledger entries in tables.

### What the numbers mean

- **Where SpacetimeDB wins.** A reducer runs inside the database with no network round trips and no row locks held
  across them. Transactions run one after another on one thread, and durability is batched. A hot row costs it
  nothing, while PostgreSQL holds the wallet's row lock for the whole transaction. Even with asynchronous commit,
  PostgreSQL stays near 2,000 transfers per second on one row, so the limit there is the lock and the MVCC row
  versions, not the disk.
- **Where PostgreSQL stands.** Our code spends most of its time on round trips: one function call instead of five
  statements gives 1.4 to 1.6 times more throughput. Part of the remaining cost is ours, not the engine's: the
  triggers that make the ledger tamper-proof (applying each entry, guarding balances, checking at commit that an
  operation sums to zero) run on every transfer. The SpacetimeDB module enforces the same rules in its reducer, but
  only for callers that go through it, so a plain PostgreSQL table without triggers would be faster than measured. Throughput falls past 64 connections on 4 cores, so a pool of
  about 16 to 32 connections is the right size here.
- **What we need.** We have not measured our load yet (BOT-2). Our assumption for 4096 players: one money operation
  per player every 10 seconds and one inventory write every 5 seconds is about 1,200 writes per second at peak.
  PostgreSQL covers that with room to spare even on this small machine. The risk is a single hot wallet: about 700
  transfers per second today. A treasury or a shop that every player pays at once should be split into several
  wallets, or its transfers batched.
- **One thread is also a limit.** In these runs every reducer executed on one thread (a single `wasm` thread in
  `top`), so writes to one database do not spread over more cores. PostgreSQL can: on a larger server its uniform
  figures grow, and SpacetimeDB's stay near one core's worth. This was not measured here (4 cores only).

### What was not measured

- Subscriptions under load: thousands of clients subscribed to queries, which is where SpacetimeDB is meant to save
  code. Nothing in Sawgrass uses that model yet, so there was nothing fair to compare.
- Larger hardware, separate client and server machines, long runs, crash recovery, and startup time with a large
  data set (SpacetimeDB loads tables into memory on start).
- Inventory, property and factions. The money ledger is the strictest of them, so the others should be easier for
  both databases. That is an assumption.

## The realtime core

The core (QUIC datagrams through gateways, the spatial grid, the 20 Hz tick) stays as it is:

- **Transport.** SpacetimeDB clients connect over a WebSocket (with an HTTP API beside it). The docs mention no
  UDP, QUIC, WebTransport or unreliable messages. Over TCP one lost packet holds back everything after it, which is
  why movement in [architecture.md](../../architecture.md) uses unreliable datagrams.
- **Update model.** Clients subscribe to SQL queries, and each transaction produces at most one update message per
  client. Our interest management sends each player a snapshot every tick, ranked and cut to a byte budget, with
  hysteresis and lower rates for far entities. Subscriptions do not offer that budget control.
- **Volume.** 4096 players at 20 Hz is 81,920 position updates per second before any fan-out. The best figure here
  was about 22,000 reducer calls per second, on 4 cores. Batching a whole tick into one call is possible, but then
  the database does the job of our interest grid. That fan-out is exactly what was not measured.

## Licence

Checked against the licence files shipped in the 2.16.1 crates.

| Part | Licence | For Sawgrass |
|---|---|---|
| Server (`spacetimedb-standalone`) | Business Source License 1.1 | Separate process, so not combined with our AGPL code. Every operator accepts its terms. |
| Module bindings (`spacetimedb` crate) | Apache-2.0 | Allowed by `deny.toml`. Our module code could stay AGPL. |
| Client SDK (`spacetimedb-sdk` crate) | Business Source License 1.1 | The world server would link it. Not on the `deny.toml` allow list. It is not an open-source licence, so a binary shipped under AGPL cannot carry it. |

The BSL's Additional Use Grant allows production use "with no more than one SpacetimeDB instance in production",
and not as a commercial "Database Service" where third parties create their own tables. Each version becomes "GNU
Affero General Public License v3.0 with a linking exception" four years after its release; for 2.16.1 the Change
Date is 2031-11-19. So production use is not forbidden, but replicas or a second instance for one server are, and
the client SDK keeps the server away from a clean AGPL build until then.

## Operations

| | PostgreSQL | SpacetimeDB (self-hosted) |
|---|---|---|
| Data location | Disk, with a cache | All tables in RAM, commit log on disk |
| Replication, failover | Streaming replicas, well-known tools | None self-hosted (its managed cloud replicates) |
| Backups, point-in-time recovery | `pg_dump`, WAL archiving | Commit log and snapshots; less tooling |
| Logic location | Our server and SQL triggers | Inside the database as reducers |
| Analytics, admin SQL | Full SQL | A subset of SQL |

An append-only ledger in RAM grows for the life of a server: here 1.7 million transfers took about 1 GB. On
PostgreSQL old history costs disk space only.

## Common claims, checked

Claims often made about SpacetimeDB, checked against its docs and licence files:

- "Clients connect over WebSocket only": **true**, apart from an HTTP API.
- "The BSL forbids production use": **not quite.** Production use is allowed with one instance and outside a
  database service. The limits that matter are the one-instance rule and the licence of the client SDK.
- "It is faster for slow roleplay state": **true**, by a wide margin. That speed is not needed at our expected
  load.
- "Adopting it means rewriting the server": true only for moving the core. For the slow state, only the wallet code
  (`sawgrass-db`) would move into a module.

## When to revisit

- BOT-2 or production shows storage writes above about 2,000 per second, or a hot wallet the game design cannot
  split.
- Scripts need live push of roleplay state to many clients, and building it on PostgreSQL turns out to be a large
  job.
- The licence changes: a version reaches its Change Date, or the client SDK moves to an open-source licence.

Worth doing either way: move `transfer` into one SQL function (1.4 to 1.6 times more throughput, measured above)
and keep system wallets that every player pays split by shop or by shard. These would be their own tasks.

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
cd stdb-client && cargo run --release -- ledger 64 20 uniform 1   # database, clients, seconds, pattern, in flight
spacetime call -s local ledger check                              # fails if the ledger is inconsistent
```

## Sources

- SpacetimeDB licence: the `LICENSE` files of the `spacetimedb` and `spacetimedb-sdk` 2.16.1 crates on crates.io.
- Connection and transport: <https://spacetimedb.com/docs/clients/connection>, <https://spacetimedb.com/docs/ws>
- Subscription semantics: <https://spacetimedb.com/docs/clients/subscriptions/semantics>
- Self-hosting and storage: <https://spacetimedb.com/docs/how-to/deploy/self-hosting>,
  <https://spacetimedb.com/docs/tables/storage-and-cache-policies>
- Vendor benchmark and a critique of it: <https://spacetimedb.com/blog/benchmarking>,
  <https://dev.to/tanay/a-closer-look-at-the-spacetimedb-v2-benchmark-is-it-really-23x-faster-than-sqlite-2l9f>.
  Their workloads differ from ours. The figures above are our own.
