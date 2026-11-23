# ADR 0003: Roleplay state in server memory with its own write-ahead log

Status: proposed, 2026-11-23

## Context

[architecture.md](../architecture.md) made PostgreSQL the source of truth for characters, money, inventory and
property, and DB-1 built the money ledger on it. The maintainers set the priority for storage: maximum
performance, with no need to keep existing code, as long as money is never created or lost and an acknowledged
change survives a crash.

DB-2 measured three designs on the DB-1 ledger with the same rules and workload
([results](../research/spacetimedb-vs-postgresql/README.md)). It used one 4-vCPU VM, 64 callers, and each transfer on
disk before the reply:

| Design | Transfers per second | One hot wallet | Restart, 1.8 M operations |
|---|---:|---:|---:|
| State in the server's memory, own log, group commit | 72,781 | 71,969 | 1.7 s |
| SpacetimeDB 2.16.1 | 11,352 | 10,707 | 13 s; 27 s after a crash |
| PostgreSQL, transfer as one SQL function | 5,666 | 917 | 0.8 s after a crash |
| PostgreSQL, current `sawgrass-db` | 3,437 | 642 | 0.8 s after a crash |

The comparison was checked for fairness. PostgreSQL already shared one WAL flush between about 13 commits.
Adding `commit_delay` did not help, and even with durability off it reached only 8,417 transfers per second
(2,196 on one wallet). Its limit is per-transaction work, row locks and round trips, not the disk. SpacetimeDB
also batches flushes; turning its durability wait off changed little (12,451). The in-process design flushing once
per transfer did about 6,000 per second, close to PostgreSQL. Its lead comes from batching flushes and from having no
network, protocol or WASM layer between the caller and the state. A kill -9 under load lost no acknowledged
transfer and left the ledger consistent.

## Decision

- **Roleplay state with value lives in the world server's memory.** Money, inventory, property and factions are
  owned by writers inside the world server process. Each kind of state, or a shard of it, has one writer. Callers
  (the tick, scripts) submit changes and get a result; nothing else mutates that state.
- **Durability comes from our own write-ahead log.** Each operation is one self-contained record (length, checksum,
  all of its entries) appended to the log. A change is acknowledged only after the log is flushed to disk
  (`fdatasync`). Flushes use group commit: one flush for every change waiting at that moment.
- **Snapshots bound restart time.** The writer periodically writes a snapshot of its state with the log position it
  covers. Restart loads the latest valid snapshot and replays the log after it, dropping a torn last record.
- **The log is copied off the machine.** Records are shipped to a second machine. Money changes wait for that copy
  before they are acknowledged, if measurement shows the cost is acceptable; otherwise the window of loss is stated
  and bounded.
- **PostgreSQL becomes the archive.** It is fed from the log asynchronously and serves history, admin queries,
  analytics and backups. It never decides a balance and is never on the path of a game action.
- **No reads of state that is not yet durable.** With group commit a change is applied in memory before its flush.
  Results and reads that leave the writer (a reply to a script, a client update) are released only after the flush
  that covers them.
- **The realtime core is unchanged.** Positions and other per-tick entity state are not written to this log.

## Consequences

- `docs/architecture.md` names the world server as the owner of roleplay state and PostgreSQL as its archive.
- We own a storage engine: the log format, checksums, recovery, snapshots, log rotation and shipping. Each needs
  its own tests: crash tests at random points, fuzzed and truncated logs, and replay compared with the PostgreSQL
  archive.
- One process holds the truth. Its memory must fit all roleplay state; DB-2 used about 82 bytes of log per transfer,
  and memory use was not measured.
- `sawgrass-db` (DB-1) is replaced as the authority. Its schema and invariants become the archive's, and the ledger
  rules (idempotency key, no negative character wallet, entries summing to zero) move into the writer.
- SpacetimeDB is not adopted. It is slower than this design, adds a network hop from the world server, and its BSL
  allows one production instance, which rules out replicas and zone splits.

Follow-up tasks, each its own issue: the log and recovery with crash tests; snapshots; the money writer replacing
`sawgrass-db` as the authority; log shipping to a second machine; the PostgreSQL archive feed; and a repeat of the
DB-2 measurement on larger hardware with BOT-2 load.

## Not measured yet

Hardware larger than 4 cores, a real power cut (the crash test killed the process, which leaves the OS cache intact),
runs longer than 30 seconds, memory use, the cost of shipping the log before acknowledgement, snapshot time, and
inventory, property and factions. This ADR rests on one prototype of about 430 lines and single runs with about 20%
spread between them.
