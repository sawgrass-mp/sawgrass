# ADR 0004: Roleplay state in server memory with its own write-ahead log

Status: proposed, 2026-11-23

## Context

[architecture.md](../architecture.md) made PostgreSQL the source of truth for characters, money, inventory and
property, and DB-1 built the money ledger on it. The maintainers set the priority for storage: maximum
performance, with no need to keep existing code, as long as money is never created or lost and an acknowledged
change survives a crash.

DB-2 measured three designs on the DB-1 ledger with the same rules and workload
([results](../research/spacetimedb-vs-postgresql/README.md), figures as of commit `664d8a9`). It used one 4-vCPU VM,
64 callers, and each transfer on disk before the reply:

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
per transfer did about 6,000 per second, close to PostgreSQL. Its lead comes from batching flushes and from having
no network, protocol or WASM layer between the caller and the state, so the table compares architectures, not
storage engines alone. Single runs varied by up to about 20%.

Crash evidence, stated exactly: the prototype recorded the number of durable operations after each flush and before
sending any reply. In three runs killed with SIGKILL after 2, 5 and 9 seconds of load, the replayed log held at
least that many operations each time, and the ledger invariants held. A process kill leaves the operating system's
page cache intact, so this tests the recovery logic and the order of flush and reply, not a power cut. Durability
against a power cut rests on `fdatasync` before each reply and was not tested by cutting power.

## Decision

- **One writer and one log for all state with value.** Money, inventory, property and factions are owned by a
  single writer inside the world server process. Callers (the tick, scripts) submit operations and get a result;
  nothing else mutates that state. An operation that touches several kinds of state, such as buying an item (money
  out, item in), is one log record and is applied whole or not at all. The prototype measured about 72,000
  operations per second with one writer. Splitting the writer later, if load demands it, needs its own ADR that keeps
  every operation atomic across the parts.
- **Durability comes from that write-ahead log.** Each operation is one self-contained record (length, checksum, all
  of its effects). A change is acknowledged only after the log is flushed to disk (`fdatasync`), and a new log file's
  directory is flushed too. Flushes use group commit: one flush for every operation waiting at that moment.
- **The log is copied to a second machine before acknowledgement.** An operation is acknowledged only when its record
  is on the local disk and has been received by a second machine. Group commit applies to the copy as well. If
  measurement shows this cost is too high, a later ADR may relax it, and must state how much acknowledged data a lost
  disk can then lose.
- **Recovery rule.** Start-up loads the latest valid snapshot and replays the log after it. Only the last record may
  be torn: cut short, or failing its checksum while ending exactly at the end of the log. That record was never
  acknowledged and is dropped. The torn-tail decision never rests on unchecked bytes: a record's length carries its
  own checksum, and a bad length is corruption, not a torn tail. A record that fails its checksum, decoding or consistency checks anywhere else, or
  an invalid snapshot, stops start-up with an error for an operator. Nothing is truncated silently.
- **Snapshots bound restart time.** The writer periodically writes a snapshot of its state with the log position it
  covers.
- **Idempotency keys live in a bounded window.** A retry with the same key and the same parameters returns the
  original result, as in DB-1; the same key with other parameters is an error. Keys are kept for a defined retry
  period, not forever, so memory stays bounded.
- **No reads of state that is not yet durable.** With group commit an operation is applied in memory before its
  flush. Results and reads that leave the writer (a reply to a script, a client update) are released only after the
  flush and copy that cover them.
- **PostgreSQL becomes the archive.** It is fed from the log asynchronously and serves history, admin queries,
  analytics and backups. It never decides a balance and is never on the path of a game action.
- **The realtime core is unchanged.** Positions and other per-tick entity state are not written to this log.

## Alternative considered: RocksDB under the same writer

DB-4 (#47, #48) kept the same writer, checks and group commit and replaced our log with an embedded engine as the store.
At 64 callers, durable: our log 72,781 transfers per second, RocksDB 51,542 (p99 2.2 ms), LMDB 36,968, redb 29,142,
SQLite with `synchronous=FULL` 24,042. At 256 callers RocksDB reached 65,905 against 93,862, and LMDB 69,849.

RocksDB gives about 70% of our log's throughput, and in return its format, checksums, recovery and compaction are
a mature engine's job rather than ours, and balances stay on disk, so restarts need no snapshots. It does not
remove the rest of the work: the copy to a second machine, no reads before durability, and the idempotency window.
It compiles C++ at build time and needs clang.

This ADR keeps our own log, because the maintainer's stated priority is maximum performance and the 30% is
measured. The answer would change to RocksDB if:

- the log task (crash tests at random points, power-cut tests, fuzzed logs) turns out larger than one or two
  tasks, or finds faults a mature engine would not have;
- the copy to a second machine or the tick-batched hand-over to the writer becomes the bottleneck, so the store's
  speed no longer decides throughput;
- a repeat on larger hardware with BOT-2 load shows the gap below about 15%.

Both are far above the expected peak of about 1,200 storage writes per second, so the 30% is headroom, not a need.
The choice follows the maintainer's priority, and a reviewer who weighs maintenance higher should argue for RocksDB
here.

Both designs keep the same writer interface, so the store behind it can be swapped later without changing game
code.

## Consequences

- `docs/architecture.md` names the world server as the owner of roleplay state and PostgreSQL as its archive.
- We own a storage engine: the log format, checksums, recovery, snapshots, log rotation, the copy to a second
  machine and the idempotency window (RocksDB would take the first four; see the alternative above). Each needs
  its own tests: crash tests at random points, power-cut tests, fuzzed and truncated logs, and replay compared with
  the PostgreSQL archive.
- One process holds the truth. Its memory must fit all state with value. DB-2 used about 82 bytes of log per
  transfer; memory use was not measured.
- A record that was durable and later rotted at the very end of the log looks like a torn one. Telling them apart
  needs a durable note of how far the log was flushed; the log task decides how.
- `sawgrass-db` (DB-1) is replaced as the authority. Its schema and invariants become the archive's, and the ledger
  rules (idempotency, no negative character wallet, entries summing to zero) move into the writer.
- SpacetimeDB is not adopted. It is slower than this design, adds a network hop from the world server, and its BSL
  allows one production instance, which rules out replicas and zone splits.

Follow-up tasks, each its own issue: the log and recovery with crash and corruption tests; snapshots; the writer for
money and inventory replacing `sawgrass-db` as the authority; the copy to a second machine, with its cost measured;
the PostgreSQL archive feed; and a repeat of the DB-2 measurement on larger hardware with BOT-2 load.

## Not measured yet

Hardware larger than 4 cores, a real power cut, runs longer than 30 seconds, memory use, the cost of the copy to a
second machine, snapshot time, and inventory, property and factions. The evidence is one prototype of about 480
lines and single runs.
