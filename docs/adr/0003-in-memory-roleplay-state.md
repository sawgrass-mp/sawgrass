# ADR 0003: Roleplay state in server memory, stored in RocksDB behind one writer

Status: proposed, 2026-11-23

## Context

[architecture.md](../architecture.md) made PostgreSQL the source of truth for characters, money, inventory and
property, and DB-1 built the money ledger on it. The maintainers set the priority for storage: reliability and
speed together, with no need to keep existing code. Money is never created or lost, and an acknowledged change
survives a crash.

DB-2 and DB-4 measured eleven designs on the DB-1 ledger with the same rules and workload
([results](../research/spacetimedb-vs-postgresql/README.md)). One 4-vCPU VM, 64 callers, every transfer on disk
before the reply:

| Design | Transfers per second | One hot wallet | 5-minute run, p99 |
|---|---:|---:|---:|
| One writer in the server, our own log | 72,781 | 71,969 | 73,828, 1.4 ms |
| One writer in the server, RocksDB | 51,542 | 56,738 | 55,480, 1.9 ms |
| One writer in the server, Fjall | 43,836 | 48,973 | 40,107, 3.2 ms |
| One writer in the server, LMDB | 36,968 | 38,278 | 32,941, 3.7 ms |
| One writer in the server, redb | 29,142 | 29,343 | not run |
| One writer in the server, SQLite `synchronous=FULL` | 24,042 | 21,553 | not run |
| SpacetimeDB 2.16.1 | 11,352 | 10,707 | not run |
| PostgreSQL, transfer as one SQL function | 5,666 | 917 | not run |
| PostgreSQL, current `sawgrass-db` | 3,437 | 642 | not run |

Two more were measured and not ranked, because durability was not at the same bar: SQLite with
`synchronous=NORMAL`, and TigerBeetle, which could only run in development mode without Direct I/O here.

What the measurements say:

- **The architecture matters more than the engine.** Every design with one writer in the server process, holding
  the state in memory and committing many operations with one disk flush, did 24,000 to 74,000 durable transfers
  per second with no penalty for a hot wallet. The database servers did 3,400 to 11,000, and PostgreSQL fell below
  1,000 on one hot wallet, because each transfer holds row locks across a network round trip and a flush.
- **Our own log is the fastest store, RocksDB the fastest proven one.** Over five minutes, long enough for
  RocksDB to compact its data into its lower levels without a write stall, RocksDB kept 75% of our own log's
  throughput with a p99 of 1.9 ms.
- **Both are far above the need.** The expected peak is about 1,200 storage writes per second for 4096 players
  (an estimate until BOT-2). RocksDB covers that about 45 times over.
- **Crash evidence, stated exactly.** The writer recorded how many operations were committed after each commit and
  before any reply. Processes killed with SIGKILL under load kept every acknowledged operation, for our own log
  (three runs) and for RocksDB, Fjall, LMDB, redb and SQLite `FULL` (three runs each, and three more for RocksDB
  with the recovery mode chosen below): 21 runs, none lost an acknowledged operation. With that mode, RocksDB
  refused to start on a log changed in the middle, and dropped only an unacknowledged last batch from a log cut
  short. A process kill leaves the operating system's page cache intact, so this tests the order of commit and
  reply and each engine's recovery, not a power cut.

## Decision

- **One writer for all state with value.** Money, inventory, property and factions are owned by a single writer
  inside the world server process. Callers (the tick, scripts) submit operations and get a result; nothing else
  mutates that state. The writer checks every rule against its copy in memory. An operation that touches several
  kinds of state, such as buying an item (money out, item in), is committed in one write batch and is applied whole
  or not at all. Splitting the writer later, if load demands it, needs its own ADR that keeps every operation
  atomic across the parts.
- **RocksDB is the store.** Each batch of waiting operations is one RocksDB write batch with `sync` set, so its
  write-ahead log is flushed to disk before the batch's callers get a reply (group commit). The batch holds each
  operation's record and the new value of every key it changed.
- **Recovery is RocksDB's, set strictly.** The WAL recovery mode is `TolerateCorruptedTailRecords`: an incomplete
  last record, never acknowledged, is dropped, and any other corruption stops start-up with an error for an
  operator. `paranoid_checks` stays on. Start-up loads the current values into the writer's memory; there is no log
  replay of our own and no snapshot format to maintain.
- **The write-ahead log is copied to a second machine before acknowledgement.** An operation is acknowledged only
  when its batch is on the local disk and has been received by a second machine. Group commit applies to the copy as
  well. If measurement shows this cost is too high, a later ADR may relax it, and must state how much acknowledged
  data a lost disk can then lose.
- **Idempotency keys live in a bounded window.** A retry with the same key and the same parameters returns the
  original result, as in DB-1; the same key with other parameters is an error. Keys are kept for a defined retry
  period and loaded at start-up from that period only.
- **No reads of state that is not yet durable.** With group commit an operation is applied in memory before its
  flush. Results and reads that leave the writer (a reply to a script, a client update) are released only after the
  flush and copy that cover them.
- **PostgreSQL becomes the archive.** It is fed from the operation records asynchronously and serves history, admin
  queries, analytics and backups. It never decides a balance and is never on the path of a game action. Operation
  records are removed from RocksDB once archived and older than the idempotency window.
- **The store sits behind one trait.** The writer talks to its store through a small interface (commit a batch,
  load the state), as the DB-4 harness does, so the store can be replaced without touching game code.
- **The realtime core is unchanged.** Positions and other per-tick entity state are not written to this store.

## Why not the others

- **Our own log** was 33% faster (73,828 against 55,480 over five minutes). That speed is not needed, and it would
  make us the owners of a storage engine: record format, checksums, torn-tail rules, snapshots, log rotation and
  corruption handling, each needing crash, power-cut and fuzz tests for good. It stays the planned optimisation: if
  measured load ever approaches what RocksDB delivers, a later ADR can put it behind the same trait.
- **Fjall** is pure Rust with no C++ build and measured 43,836 (40,107 over five minutes) and passed the same crash tests. It is younger and far less deployed than
  RocksDB, so it is the first replacement to reconsider once it has more production use, not the default now.
- **LMDB** is mature and simple, but slower here at 64 callers (32,941 over five minutes), needs its map size
  set in advance and is opened through an `unsafe` call.
- **redb and SQLite `FULL`** were slower (29,142 and 24,042). SQLite with `synchronous=NORMAL` is faster but can
  lose acknowledged transfers on a power cut.
- **TigerBeetle** stores only accounts and transfers, so inventory and property would need a second store and a
  purchase would span two systems. Its figures here are not representative: it could not run with Direct I/O.
- **SpacetimeDB** is about 4.5 times slower than RocksDB behind our writer, adds a network hop, and its Business Source
  Licence allows one production instance, which rules out replicas and zone splits.
- **PostgreSQL** is 9 to 15 times slower and collapses on a hot wallet. It stays as the archive.

## What would change the answer

- A repeat on larger hardware with BOT-2 load shows RocksDB, with the copy to a second machine, near the measured
  need; then our own log, behind the same trait, becomes the next step.
- RocksDB's tail latency under long load (the worst single request in the 5-minute run was 4.3 s; our own log's
  was 4.5 s on the same VM, so the disk is the likely cause) turns out to come from RocksDB on real hardware.
- The C++ build of RocksDB becomes a real cost for contributors or for release builds; then Fjall is measured
  again first.

## Consequences

- `docs/architecture.md` names the world server as the owner of roleplay state and PostgreSQL as its archive.
- A C++ dependency enters the server build: `rocksdb` compiles RocksDB through a build script and needs clang. It
  is a third-party crate, so the workspace gains no `build.rs` of its own, and it must pass `cargo deny check`.
- One process holds the truth. Its memory must fit all state with value; memory use was not measured.
- `sawgrass-db` (DB-1) is replaced as the authority. Its schema and invariants become the archive's, and the ledger
  rules (idempotency, no negative character wallet, entries summing to zero) move into the writer.
- SpacetimeDB is not adopted.

Follow-up tasks, each its own issue: the writer and its RocksDB store with crash tests at random points and
corruption tests; the writer for money and inventory replacing `sawgrass-db` as the authority; the copy to a second
machine, with its cost measured; the PostgreSQL archive feed; and a repeat of the measurement on larger hardware
with BOT-2 load.

## Not measured yet

Hardware larger than 4 cores, a real power cut, memory use, the cost of the copy to a second machine, runs longer
than five minutes, RocksDB tuning, and inventory, property and factions. All figures are single runs on one VM, which varied by up to about 20%.
