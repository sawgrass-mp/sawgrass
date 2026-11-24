# Storage for roleplay state: what we measured and what we chose

Measured 2026-11-23 with a prototype in-process ledger, SQLite 3.50.2, redb 3.1.3, RocksDB 10.4.2, Fjall 3.1.13,
LMDB 0.9.70 (the development `mdb.master` branch, as `heed` builds it), TigerBeetle 0.17.9, SpacetimeDB 2.16.1 and
PostgreSQL 16.15. Tasks: DB-2, DB-4. The decision is
[ADR 0003](../../adr/0003-in-memory-roleplay-state.md), proposed in #46.

The question: what storage for roleplay state with money in it (money, inventory, property, factions) is reliable
and fast together? Keeping existing code is not a goal. Money must never be created or lost, and an acknowledged
change must survive a crash. This page records what was measured, what was not, what we chose and why.

## Short answer

**We chose RocksDB behind one writer in the world server.** The server holds roleplay state in memory, one writer
applies every change, and each batch of changes is one RocksDB write with its log flushed to disk before anyone
gets a reply. See [What we chose and why](#what-we-chose-and-why).

| Design (same ledger, same rules, 64 callers, durable) | Transfers per second | One hot wallet | 5-minute run (p99) |
|---|---:|---:|---:|
| One writer in the server, our own log | 72,781 | 71,969 | 73,828 (1.4 ms) |
| **One writer in the server, RocksDB** | **51,542** | **56,738** | **55,480 (1.9 ms)** |
| One writer in the server, Fjall | 43,836 | 48,973 | 40,107 (3.2 ms) |
| One writer in the server, LMDB | 36,968 | 38,278 | 32,941 (3.7 ms) |
| One writer in the server, redb | 29,142 | 29,343 | not run |
| One writer in the server, SQLite `synchronous=FULL` | 24,042 | 21,553 | not run |
| SpacetimeDB 2.16.1 | 11,352 | 10,707 | not run |
| PostgreSQL, one SQL function per transfer | 5,666 | 917 | not run |
| PostgreSQL, current `sawgrass-db` code | 3,437 | 642 | not run |

"One writer in the server, X" is the same writer with the same group commit, with X storing each batch. Left out
of the table because they do not meet the same bar:

- SQLite with `synchronous=NORMAL`: 29,616 and 30,861. It can lose recent acknowledged transfers on a power cut.
- TigerBeetle: 20,781 and 22,281 with one transfer per request, 50,516 and 60,037 with 100 per request (p99 0.6
  to 0.7 s). It ran in `--development` mode without Direct I/O, so its durability against a power cut is not
  established here (see [Embedded engines and TigerBeetle](#embedded-engines-and-tigerbeetle)).

- **The architecture matters more than the engine.** Every design with one writer in the server, holding state in
  memory and committing many transfers with one disk flush (group commit), did 24,000 to 74,000 durable transfers
  per second, with no penalty for a hot wallet. The database servers did 3,400 to 11,000, and PostgreSQL fell below
  1,000 on one hot wallet.
- **Our own log is the fastest store; RocksDB is the fastest proven one.** Over five minutes, long enough for
  RocksDB to compact its data into its lower levels, it kept 75% of our own log's throughput, with no write stall.
- **Both are far above the need.** The expected peak is about 1,200 storage writes per second for 4096 players,
  an estimate until BOT-2. RocksDB covers it about 45 times over, so the extra speed of our own log is not worth
  owning a storage engine.
- **No acknowledged transfer was lost.** In kill -9 tests under load (three per store, for our own log, RocksDB,
  Fjall, LMDB, redb and SQLite `FULL`), every transfer a caller had been told was done was still there after
  restart, and the ledger balanced. RocksDB, set to drop only an incomplete last record, refused to start on a log
  damaged in the middle. These were process kills, not power cuts.
- **Group commit decides it for every store.** One commit per transfer gave 3,300 to 6,100 per second for all of
  them except SQLite `NORMAL`: the speed of the disk.

All figures come from one 4-core VM and single runs that varied by up to about 20%. Read
[What was not measured](#what-was-not-measured) before relying on them.

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
  applies the transfer and appends one record per operation (length with its own CRC32, CRC32 of the body, both
  entries) to a log file. It replies once the record is on disk. Flush modes: `each` (one `fdatasync` per
  transfer), `group` (one per batch of up to 4,096 waiting requests), `none` (no flush; a crash loses recent
  transfers). Recovery replays the log and checks the invariants. It drops only a torn last record (cut short, or
  failing its checksum and ending exactly at the end of the file); a bad record anywhere else stops start-up with
  an error, because acknowledged transfers follow it. A retry with the same idempotency key and parameters returns
  the original operation, as in `sawgrass-db`.
- **In-process ledger on an embedded engine:** [`embedded/`](embedded/src/main.rs). The same writer thread, checks
  and group commit as `inproc`, but each batch is one transaction in the engine (every operation plus the new
  balance of each wallet it touched) instead of a record in our log. Engines: SQLite in WAL mode with
  `synchronous=FULL` and with `synchronous=NORMAL`; redb with `Durability::Immediate` (set explicitly); RocksDB
  with a write batch and `sync=true`, WAL recovery mode `TolerateCorruptedTailRecords` and `paranoid_checks`;
  Fjall with a write batch and `PersistMode::SyncAll`; LMDB with default flags (synced on commit). On start it
  loads the balances and rebuilds the idempotency keys from the stored operations. With `SAWGRASS_ACK_MARKER` set,
  it records how many operations are committed after each commit and before any reply, as `inproc` does.
- **TigerBeetle:** one replica, driven by [`tigerbeetle/`](tigerbeetle/src/main.rs) through the official Rust
  client. Character accounts carry `debits_must_not_exceed_credits`, so they cannot go negative; the transfer id is
  the idempotency key. One shared client, which batches concurrent requests itself.

Workload: 10,000 characters with funded bank wallets. `uniform` moves 1 to 100 units between two random characters;
`hot` has every transfer pay the one shop wallet. Each caller sends a transfer, waits for the reply and sends the
next. Runs last 20 seconds (10 seconds for the in-process ledger, the embedded engines and TigerBeetle).

### Durability, stated for each

| Design | When the caller gets "done" | Lost on a crash |
|---|---|---|
| PostgreSQL, `synchronous_commit = on` | After the WAL record is flushed | Nothing acknowledged |
| SpacetimeDB, confirmed reads (default) | After the transaction is durable | Nothing acknowledged |
| In-process, `each` or `group` | After `fdatasync` of the log | Nothing acknowledged |
| In-process on SQLite `FULL`, redb, RocksDB, Fjall or LMDB | After the engine's commit, which syncs to disk | Nothing acknowledged |
| In-process on SQLite `NORMAL` | After the commit reaches the WAL file, before any sync | Nothing on a process crash; recent acknowledged transfers on a power cut or OS crash |
| TigerBeetle, one replica | After the replica has written the transfer to its own log | Nothing acknowledged on a process kill; against a power cut not established, because it ran without Direct I/O (see below) |
| In-process `none`, PostgreSQL `synchronous_commit = off`, SpacetimeDB confirmed reads off | Before the flush | Recent acknowledged transfers, always whole ones |

The headline figures use only the rows where nothing acknowledged is lost.

## Results

Machine: one cloud VM, 4 vCPU (Intel Xeon 2.1 GHz), 16 GB RAM, virtual disk (`open_datasync` about 8,500 flushes
per second). Database, server and callers all ran on this one machine. Single runs: run-to-run spread was up to
about 20% (see [Was the comparison fair?](#was-the-comparison-fair)), so treat smaller differences as noise.

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
| In-process | 1.7 s | Replays the whole 158 MB log (86 bytes per operation) after dropping the page cache; grows with the log until snapshots exist |
| SpacetimeDB | 27 s after a crash, 13 s after a clean stop | Loads all tables into memory; an empty server starts in 0.15 s |
| PostgreSQL | 0.8 s after a crash | Redoes only the WAL since the last checkpoint; data stays on disk |

Crash tests of the in-process ledger. With `SAWGRASS_ACK_MARKER` set, the writer records how many operations are
durable after each flush and before it sends any reply, so every acknowledgement a caller ever received is covered
by that number. The process was killed with SIGKILL after 2, 5 and 9 seconds of `group` load at 64 callers:

| Killed after | Operations in the log | Covered by acknowledgements | Invariants |
|---:|---:|---:|---|
| 2 s | 84,966 | 84,902 | hold |
| 5 s | 235,110 | 235,055 | hold |
| 9 s | 327,608 | 327,608 | hold |

The log never held fewer operations than were acknowledged. The few extra operations were on disk but not yet
answered. (These runs shared the machine with a compiler, so their throughput is lower than in the tables above.
With the machine idle, the final version of the prototype did 73,272 tps uniform and 70,517 hot at 64 callers.) A
process kill leaves the OS page cache intact, so these test the recovery logic and the ordering of flush and reply,
not a power cut. For a power cut the guarantee rests on `fdatasync` before the reply (and an `fsync` of the
directory when the log is created), which was not tested by cutting power.

Corruption tests on the 9-second log. Each record's length has its own checksum, so deciding that the last record
was cut short never rests on unchecked bytes.

| Damage | Result |
|---|---|
| One byte of a record's length changed, mid-log | Start-up refused: "corrupt record header at byte 13911279" |
| One byte of a record's body changed, mid-log | Start-up refused: "corrupt record at byte 13911279" |
| Log cut 30 bytes short | Last record dropped as torn, invariants hold |
| One byte of the last record changed | Last record dropped as torn, invariants hold |

A limit of the last rule: a last record that was durable and later rotted looks the same as a torn one and is
dropped. A real engine needs a durable note of how far the log was flushed to tell them apart.

### Embedded engines and TigerBeetle

Same machine, same workload, run with the machine otherwise idle. Transfers per second (p99 latency), 10-second
runs, `group` commit unless noted; our own log repeated from the table above for comparison:

| Pattern | Callers | Own log | RocksDB | Fjall | LMDB | redb | SQLite `FULL` | SQLite `NORMAL`, not ranked | TigerBeetle, dev mode, not ranked |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| uniform | 1 | 5,444 (0.42 ms) | 4,643 (0.52 ms) | 5,055 (0.64 ms) | 2,688 (0.81 ms) | 3,052 (0.76 ms) | 4,563 (0.67 ms) | 12,649 (0.25 ms) | not run |
| uniform | 16 | 44,718 (0.78 ms) | 33,362 (1.0 ms) | 26,066 (1.4 ms) | 16,319 (2.3 ms) | 13,845 (2.4 ms) | 15,764 (3.2 ms) | 26,046 (7.9 ms) | not run |
| uniform | 64 | 72,781 (2.0 ms) | 51,542 (2.2 ms) | 43,836 (3.2 ms) | 36,968 (3.0 ms) | 29,142 (4.0 ms) | 24,042 (6.8 ms) | 29,616 (12 ms) | 20,781 (32 ms) |
| uniform | 256 | 93,862 (4.0 ms) | 65,905 (6.1 ms) | 57,351 (7.5 ms) | 69,849 (5.8 ms) | 51,928 (8.2 ms) | 27,617 (19 ms) | 31,334 (21 ms) | 30,016 (48 ms) |
| hot | 64 | 71,969 (1.7 ms) | 56,738 (2.3 ms) | 48,973 (2.7 ms) | 38,278 (3.1 ms) | 29,343 (4.6 ms) | 21,553 (7.4 ms) | 30,861 (12 ms) | 22,281 (30 ms) |
| hot | 256 | 97,107 (4.1 ms) | not run | not run | not run | not run | not run | not run | 33,492 (47 ms) |
| uniform, one commit per transfer | 64 | 6,128 (16 ms) | 5,357 (21 ms) | 5,236 (21 ms) | 3,322 (34 ms) | 3,315 (33 ms) | 4,855 (25 ms) | 14,791 (12 ms) | |

TigerBeetle with 100 transfers per request at 64 callers: 50,516 uniform (p50 98 ms, p99 694 ms) and 57,690 to
60,037 hot (p99 604 ms). Its invariant check (debits equal credits, no character overdrawn) passed after every run.

**Five minutes under load.** A 10-second run writes about 100 MB, too little for an LSM engine (RocksDB, Fjall)
to compact its data into its lower levels, which is where write stalls and long-run slowdowns show. So the four
fastest stores ran for 300 seconds at 64 callers, uniform:

| Store | Transfers per second | p50 | p99 | Worst request | Stored | Restart after |
|---|---:|---:|---:|---:|---:|---:|
| Own log | 73,828 | 0.82 ms | 1.4 ms | 4.5 s | 22.2 M operations, 1.9 GB | not run |
| RocksDB | 55,480 | 1.07 ms | 1.9 ms | 4.3 s | 16.7 M operations, 1.4 GB | 22.3 s |
| Fjall | 40,107 | 1.39 ms | 3.2 ms | 2.3 s | 12.1 M operations, 1.1 GB | 17.9 s |
| LMDB | 32,941 | 1.83 ms | 3.7 ms | 2.7 s | 9.9 M operations, 0.9 GB | 10.9 s |

An earlier 300-second RocksDB run gave 50,097 (p99 2.3 ms). In it RocksDB completed 13 compactions, its data
reached levels 5 and 6, and its own statistics reported no write stall. Every store, our own log included, had a
worst single request of 2 to 4.5 seconds over five minutes on this VM, which points at the virtual disk rather
than the engines; it was not investigated further. Restart times here are long because this harness rebuilds the
idempotency keys from every stored operation, about one second per million; a real server keeps a bounded window,
and these stores keep balances on disk, so there is nothing else to replay.

**Crash tests with acknowledgements.** With `SAWGRASS_ACK_MARKER` set, each store was killed with SIGKILL after 2, 5
and 9 seconds of `group` load at 64 callers, then restarted with the page cache dropped:

| Store | Killed after 2 s | 5 s | 9 s |
|---|---|---|---|
| RocksDB | 110,811 stored, 110,748 acknowledged | 257,679 / 257,615 | 456,264 / 456,264 |
| RocksDB, strict recovery mode | 110,858 / 110,794 | 250,256 / 250,193 | 453,668 / 453,668 |
| Fjall | 117,498 / 117,436 | 275,871 / 275,807 | 448,736 / 448,672 |
| LMDB | 85,060 / 85,060 | 179,383 / 179,383 | 322,022 / 322,022 |
| redb | 61,930 / 61,930 | 135,000 / 135,000 | 248,357 / 248,295 |
| SQLite `FULL` | 58,894 / 58,894 | 129,798 / 129,798 | 212,432 / 212,368 |

In all 18 runs every acknowledged operation was stored, and the invariants held. (The first five rows ran before
RocksDB's recovery mode was set; the second row is the same test with it set.) As for our own log, a process kill
leaves the page cache intact, so these test commit-before-reply and each engine's recovery, not a power cut.

**RocksDB with a damaged log**, recovery mode `TolerateCorruptedTailRecords`, on a log of 24.6 MB left by a kill
after 4 seconds (202,488 operations stored, 202,426 acknowledged):

| Damage | Result |
|---|---|
| None | Starts; 202,488 operations, invariants hold |
| One byte changed in the middle of the log | Start-up refused: "Corruption: checksum mismatch" |
| Log cut 30 bytes short | Starts; the incomplete last batch (62 operations, never acknowledged) is dropped, invariants hold |
| One byte changed 10 bytes before the end, inside the last batch | Start-up refused: "Corruption: checksum mismatch" |

This is stricter than our own log: a last record that is complete in length but fails its checksum is refused,
not dropped, even though here it was never acknowledged. A power cut that leaves such a record stops the server
until an operator acts, with nothing acknowledged lost. The operator restores from the second machine's copy; ADR
0003 rules out restarting in RocksDB's more lenient `PointInTimeRecovery` mode, which after damage in the middle
of the log would silently drop acknowledged operations.

**Restart after a short run**, page cache dropped: RocksDB 1.17 s for 1,269,388 operations (104 MB), LMDB 0.60 s
for 886,233 (81 MB), redb 0.68 s for 712,429 (114 MB), SQLite `FULL` 0.23 s for 303,867 (31 MB), SQLite `NORMAL`
0.50 s for 716,994 (72 MB). TigerBeetle: 3.0 s after kill -9, 1.3 s after SIGTERM (2.8 GB preallocated file), with
its invariants intact after the kill.

What to keep in mind about these rows:

- **TigerBeetle did not run as it is meant to.** This VM's disk does not support Direct I/O, which TigerBeetle uses
  to make a write durable when it completes, so it ran with `--development`; its own help says that mode "should
  not be used for production or benchmarks". It ran as one replica on the same machine as the callers. Production
  runs six replicas, which adds network round trips to every commit and adds fault tolerance. Treat its figures as
  a rough indication only. It also holds only accounts and transfers, so inventory and property would live in a
  second store, and a purchase would span two systems.
- **The B-tree and LSM engines gain more from more callers** than our log does, so the gap narrows at 256 callers
  (RocksDB 70%, LMDB 74% of our log).
- **SQLite `NORMAL` is the fast mode people know SQLite for.** In WAL mode it skips the sync at commit and syncs
  at checkpoints, which is safe against a process crash but not a power cut. Its single-caller figure is high for
  that reason. It also hit a ceiling near 31,000 at 256 callers.
- **Building them.** SQLite (bundled) and RocksDB compile C and C++ code during the build, through their `-sys`
  crates' build scripts; RocksDB also needs clang and adds minutes to a clean build. LMDB is a small C library.
  Fjall and redb are pure Rust.

### Was the comparison fair?

The in-process `group` mode batches flushes, and `each` (one flush per transfer, about 6,000 per second) is close
to the PostgreSQL figures. So the question is whether the other two already had an equivalent batching.

- **PostgreSQL already batches flushes.** Concurrent commits share one WAL flush. Measured with `pg_stat_wal`
  during a 10-second run of the one-function variant at 64 callers: 68,467 commits, 5,178 WAL flushes, about 13
  commits per flush. That run gave 6,844 tps (the table shows 5,666 from an earlier run: run-to-run spread is about
  20%). Adding an explicit delay to gather more commits per flush (`commit_delay = 1000`, `commit_siblings = 5`)
  did not help: 5,170 tps uniform and 478 tps hot at 64 callers. Turning durability off (`synchronous_commit =
  off`) gave 8,417 uniform and 2,196 hot. So the disk is not what limits PostgreSQL here; per-transaction work,
  locks and round trips are.
- **SpacetimeDB already batches flushes too.** Its commit log is not flushed once per transaction, and turning
  confirmed reads off (reply before durability) changed little: 12,451 against 11,352 tps. Keeping 16 calls in
  flight per connection, so that more work arrives at once, raised it to 21,822 tps with a p99 of 113 ms.
- **What remains unequal:** the in-process callers are threads in the same process, with no network, protocol or
  WASM between them and the ledger. That is the point of the design rather than a flaw in the test, but it means
  the gap measures the whole path, not storage alone. Without any flush the in-process ledger did 110,806 tps, so
  its flush with batching costs about a third of its throughput.

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
- **Our expected load** is a guess until BOT-2: about 1,200 storage writes per second at peak for 4096 players. Every
  design here covers that on even load; PostgreSQL does not cover it on one hot wallet.

### What was not measured

- Machines larger than 4 cores, callers on another machine, runs longer than five minutes, and a real power cut.
- Reads under write load, subscriptions (SpacetimeDB's push of changes to clients), and memory use of the
  in-process ledger. It keeps every idempotency key in memory as built; a real one keeps a bounded window.
- Inventory, property and factions. The ledger is the strictest of them; that they behave alike is an assumption.
- The asynchronous PostgreSQL archive and the copy to a second machine. Neither exists yet.
- For the embedded engines: tuning beyond the settings named above (RocksDB, Fjall and LMDB all have many),
  memory use, and the hot pattern over five minutes.
- TigerBeetle with Direct I/O, with several replicas, or with callers on other machines.

## What we chose and why

**RocksDB behind one writer in the world server**, recorded in
[ADR 0003](../../adr/0003-in-memory-roleplay-state.md) (proposed in #46).

- **One writer owns all state with value** (money, inventory, property, factions) inside the world server process.
  It checks every rule against its copy in memory, so a hot wallet costs nothing, and an action that touches
  several kinds of state, such as buying an item, is one write batch that is applied whole or not at all.
- **Each batch of waiting operations is one RocksDB write with its log flushed to disk** (group commit) before any
  caller in the batch gets a reply. Measured: 51,542 to 56,738 transfers per second in 10-second runs and 55,480
  over five minutes, p99 under 2 ms, about 45 times the expected peak.
- **Recovery is RocksDB's, set strictly**: an incomplete last record, never acknowledged, is dropped; any other
  damage stops start-up. Tested above with kills and a damaged log.
- **Copied to a second machine before acknowledgement**, so a dead disk loses nothing acknowledged. Not built or
  measured yet.
- **No reads of state that is not yet durable**: results leave the writer only after the flush that covers them.
- **PostgreSQL becomes the archive**, fed asynchronously for history, admin queries and backups. It never decides
  a balance.
- **The store sits behind one small interface** (commit a batch, load the state), so it can be replaced without
  touching game code.

Why this one: the requirement is reliability and speed together. The measurements show that the speed comes from
the architecture (one writer, state in memory, group commit), not from the store: every store behind that writer
was 2 to 20 times faster than the database servers. Among the stores, RocksDB is the fastest that we do not have
to write and maintain ourselves. It has been in heavy production use for over a decade, and its recovery behaved
exactly as required in our tests.

### Why not the others

| Option | Measured (64 callers, durable) | Why not |
|---|---:|---|
| Our own log | 72,781; 73,828 over 5 min | 33% faster, but the need is about 1,200 writes per second, so the extra speed buys nothing the game needs. In return we would own a storage engine for good: record format, checksums, torn-tail rules, snapshots, log rotation and corruption handling, each with crash, power-cut and fuzz tests. Kept as the planned optimisation behind the same interface, if measured load ever approaches what RocksDB delivers. |
| Fjall | 43,836; 40,107 over 5 min | Pure Rust with no C++ build, and it passed the same crash tests. Slower than RocksDB here and far younger, with much less production use. The first replacement to reconsider if the C++ build becomes a burden. |
| LMDB | 36,968; 32,941 over 5 min | Mature and simple, but slower at 64 callers, needs its maximum size set in advance, and is opened through an `unsafe` call. |
| redb | 29,142 | Pure Rust, passed the crash tests, but about half of RocksDB's throughput. |
| SQLite, `synchronous=FULL` | 24,042 | Reliable, but the slowest store here and flat beyond 64 callers. |
| SQLite, `synchronous=NORMAL` | 29,616 | Can lose acknowledged transfers on a power cut. |
| TigerBeetle | 20,781 (dev mode) | Holds only accounts and transfers, so inventory and property would need a second store and a purchase would span two systems. Could not run with Direct I/O here, so its figures are not representative. |
| SpacetimeDB | 11,352 | About 4.5 times slower than RocksDB behind our writer, a network hop from the world server, and its Business Source Licence allows one production instance (see [Licences](#licences)). |
| PostgreSQL | 3,437 to 5,666; under 1,000 on one hot wallet | Row locks held across a network round trip and a flush. Kept as the archive. |

### What would change the answer

- A repeat on larger hardware with BOT-2 load shows RocksDB, together with the copy to a second machine, close to
  the measured need. Then our own log, behind the same interface, is the next step.
- The worst-case requests of several seconds seen over five minutes turn out to come from RocksDB rather than the
  VM's disk on real hardware.
- RocksDB's C++ build becomes a real cost for contributors or releases. Then Fjall is measured again first.

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

## Licences

Checked against the licence files of the crates and sources used. Sawgrass is AGPL-3.0, and #44 proposes MIT OR
Apache-2.0.

| Engine | Licence | Fits AGPL-3.0 and MIT OR Apache-2.0 |
|---|---|---|
| Fjall | MIT OR Apache-2.0 | Yes |
| SQLite | Public domain; `rusqlite` and `libsqlite3-sys` MIT | Yes |
| redb | MIT OR Apache-2.0 | Yes |
| RocksDB | GPL-2.0 OR Apache-2.0, used under Apache-2.0 (parts from LevelDB, BSD-3-Clause); `rocksdb` crate Apache-2.0 | Yes |
| LMDB | OpenLDAP Public License 2.8, a BSD-style licence; `heed` MIT, `lmdb-master-sys` Apache-2.0 | Yes |
| TigerBeetle | Apache-2.0, server and Rust client | Yes |
| PostgreSQL | PostgreSQL Licence, a BSD-style licence | Yes |
| SpacetimeDB | Business Source License 1.1 for the server and SDK; details below | Use limit, see below |

None of the open-source engines limits how many instances run or who runs them. SpacetimeDB is the exception.

### SpacetimeDB

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

In-process ledger on an embedded engine (RocksDB's build needs clang; set `LIBCLANG_PATH` if it is not found):

```sh
cd embedded
cargo run --release -- run rocksdb /tmp/ledger-rocksdb 64 10 uniform group   # engine, dir, callers, seconds, pattern, each|group
cargo run --release -- recover rocksdb /tmp/ledger-rocksdb                   # load time and invariants
```

Engines: `sqlite-full`, `sqlite-normal`, `redb`, `rocksdb`, `fjall`, `lmdb`. Use a new directory per engine. For
a crash test, set `SAWGRASS_ACK_MARKER=<dir>/ack` for both commands, kill the `run` with SIGKILL, then `recover`
prints `acknowledged=N all present` or `LOST`.

TigerBeetle (binary from <https://github.com/tigerbeetle/tigerbeetle/releases>; drop `--development` on a disk
with Direct I/O):

```sh
tigerbeetle format --cluster=0 --replica=0 --replica-count=1 --development /tmp/0_0.tigerbeetle
tigerbeetle start --addresses=3010 --development /tmp/0_0.tigerbeetle &
cd tigerbeetle
cargo run --release -- seed 3010
cargo run --release -- run 3010 64 10 uniform 1    # address, callers, seconds, pattern, transfers per request
cargo run --release -- check 3010                  # fails if the ledger is inconsistent
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
