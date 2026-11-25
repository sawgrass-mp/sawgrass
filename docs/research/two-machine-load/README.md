# 3,800 and 4,096 players on the project's own servers

Measured 2026-11-25 on two dedicated machines, in three series. Server and bots were built from `main` at
SRV-10, NET-12 and BOT-4 for series 1 and 2, and for series 3 from the BOT-6 branch (#141, commit 23c0266), which was not merged when the runs were made. Task: DOC-12 (#151).

The [load-test bots page](../load-test-bots/README.md) measured the server on one 4-vCPU VM, with the bots
sharing the machine, and stopped at 3,000 players. This page takes the same server and bots to the plan's
two load gates: phase 1 (3,000 bots in a crowd, tick p99 under 35 ms) and phase 2 (4,096 bots from a second
machine). The bots speak the real protocol through the QUIC gateway. They are described in the
[bots README](../../../crates/sawgrass-bots/README.md).

## Short answer

- **3,800 players in a city-density crowd fit with room to spare.** Two runs gave tick p99 24.4 and 23.8 ms against
  the 35 ms budget (70% of the 50 ms tick at 20 Hz), with no overrun and 1.41 to 1.42 server cores. The load
  generator was not saturated in either. They meet phase 1's gate; see
  [What this means for the gates](#what-this-means-for-the-gates).
- **4,096 players run with no overrun, but two server cores are the limit.** With one gateway thread the tick held
  p99 28.4 to 30.5 ms, but the gateway dropped 59 to 98 whole ticks of snapshots: bots received 17.6 to 18.6
  snapshots a second out of 20. With two gateway threads sharing the same two cores, every snapshot arrived, but
  tick p99 rose to 38.0 to 39.1 ms, over the budget, because the tick thread now competes for a core. No run had a
  tick over 50 ms.
- **The tick is not the limit; the gateway is, on two cores.** At 4,096 players the tick thread used 0.48 to 0.51
  of a core and the gateway the other 1.0 core, about 6.5 µs per datagram for about 155,000 datagrams a second in
  both directions. That matches the 4.7 to 9.4 µs per datagram measured in BOT-3.
- **Only series 2 and 3 count.** In series 1 the bots ran on a 2-core machine across the network and saturated it
  at 2,000 bots. Their acknowledgements came late, so the server sent deltas against old baselines (35 to 47 kB/s
  per player instead of about 12) and refused handshakes. That series measured the generator, not the server.
- **Not measured**: 30 Hz (SRV-8 is not done; every run is at 20 Hz), a server with more than two cores at
  4,096 players (no generator could drive it, see [Limits of the stand](#limits-of-the-stand)), real internet
  latency for most bots, and runs longer than 40 s.

## Machines and method

| Machine | Cores | Role |
|---|---|---|
| A | 4 | Server pinned to cores 0–1 (`taskset`), local bots on cores 2–3 over loopback |
| B | 2 | Remote bots, across the network |

- **Server**: `sawgrass-server --listen 0.0.0.0:4433 --stats-to-clients --max-per-address 5000`, with
  `--net-threads 1 --tick-threads 1` unless the table says otherwise.
- **Bots**: `sawgrass-bots run --area 1500 --duration 40 --report-every 40 --connect-per-sec 200`, one seed per run.
  The 1,500 m square is the dense crowd of the plan's gates, with 118 to 128 entities in each snapshot. Series 3
  ran machine A's bots with `--sockets 32 --decoders 32` and machine B's with `--sockets 8 --decoders 8` (BOT-6).
  That leaves what the server receives and sends
  unchanged and makes the generator cheap enough not to saturate.
- **Which runs count**: a run counts only if every generator's report says `ok`, not `SATURATED`. Figures come from
  the bots' "whole load phase" report: the server's tick percentiles and CPU, and what the bots measured. Runs were
  alternated between configurations.

## Results

### Series 2: 3,800 and 4,096, one gateway thread

Machine A: 3,400 or 3,700 bots; machine B: 396 to 450 bots.

| Run | Players | Tick p50 / p99 / max | Overruns | Snapshots/s per bot | Server CPU (tick) | Generator A | Counts |
|---|---:|---:|---:|---:|---:|---|---|
| 3,800, seed 3 | 3,793 | 19.7 / 24.4 / 25.9 ms | 0 | 19.3 | 1.42 (0.42) | 1.69 of 2, ok | yes |
| 3,800, seed 4 | 3,797 | 17.9 / 23.8 / 26.3 ms | 0 | 19.9 | 1.41 (0.41) | 1.69 of 2, ok | yes |
| 4,096, seed 1 | 4,090 | 22.9 / 28.6 / 35.3 ms | 0 | 16.9 | 1.48 (0.48) | 1.73 of 2, SATURATED | no |
| 4,096, seed 2 | 4,093 | 22.5 / 27.5 / 28.7 ms | 0 | 17.1 | 1.48 (0.48) | 1.73 of 2, SATURATED | no |

The 3,800 runs had 1 to 7 bots that never connected ("handshake timed out"). These bots made one attempt each,
since this binary predates BOT-6's retries. No bot dropped once connected.

### Series 3: 4,096 with light bots, one or two gateway threads

Machine A: 3,700 bots with `--sockets 32 --decoders 32`; machine B: 396 bots. Every bot connected and none
dropped. Every generator was `ok`, at 1.49 to 1.68 of 2 cores on machine A and 0.52 to 0.63 on machine B.

| Run | Gateway threads | Tick p50 / p99 / max | Overruns | Snapshots/s per bot | Dropped ticks | Server CPU (tick) |
|---|---:|---:|---:|---:|---:|---:|
| seed 1 | 1 | 23.5 / 30.5 / 34.4 ms | 0 | 17.6 | 98 | 1.51 (0.51) |
| seed 2 | 1 | 22.4 / 28.4 / 31.4 ms | 0 | 18.6 | 59 | 1.48 (0.48) |
| seed 1 | 2 | 23.2 / 39.1 / 47.9 ms | 0 | 20.0 | 0 | 1.86 (0.49) |
| seed 2 | 2 | 23.0 / 38.0 / 42.3 ms | 0 | 20.0 | 0 | 1.85 (0.47) |

"Dropped ticks" is the server's count of ticks whose snapshots it dropped because the gateway had not sent the
previous two yet (SRV-10's overload policy). The tick continued on time. Bots received each player's 12 to 13 kB/s,
the same as at 3,800.

### Series 1: a saturated generator (does not count)

All 2,000 to 4,096 bots ran on machine B, with up to 1,096 more on machine A's spare core. Machine B's generator
was `SATURATED` in every run, at 1.89 to 1.95 of 2 cores, with state sends late by 0.5 to 2 s at p50. Its bots
sent about a tenth of their states, so their acknowledgements lagged. The server then sent each player 35 to
47 kB/s of deltas against old baselines, against about 12 kB/s when acknowledgements keep up. That extra traffic
cost the generator more again. 20 to 320 bots per run were refused at the handshake. The cause was the 256
handshake slots of NET-12, each held for over a second by the slow generator, with bots that did not retry. BOT-6
raises the cap to 1,024 and makes bots retry. The server's figures from this series (tick p99 9 to 16 ms) are low
because the bots barely loaded it, and they must not be quoted.

## What this means for the gates

- **Phase 1's gate is met.** It asks for 3,000 bots in the 1.5 km square with no dropped bots and tick p99 under
  35 ms, on a dedicated 4-core machine with the server on two of its cores. Series 2 ran 3,800 bots on a 4-core machine with half its cores for the server: p99
  23.8 to 24.4 ms, no dropped bots. Machine A's spare core also carried 2,500 light bots against a 3-core server
  at p99 8.2 ms. On the shared cloud VM of the BOT-3 page, 3,000 bots measured 37 to 38 ms p99 on 2026-11-25 (BOT-6
  runs), over the budget, for reference. There the bots and the server share hyperthreads of one 4-vCPU guest. The
  gate named that VM until this report; the maintainer moved it to a dedicated machine, because a shared VM's
  figures are noisy and cannot be reproduced.
- **Phase 2's gate: not met yet.** It asks for 4,096 bots on a second machine. Here 3,700 of the 4,096 ran on the
  server's own machine and only 396 on the second one. No configuration kept tick p99 within budget without losing
  snapshots: one gateway thread dropped 7 to 12% of them, two kept them all at p99 38 to 39 ms. The runs had no
  overrun, and they show the gateway, not the tick, as the limit on two cores. Phase 2's tasks NET-10 and SRV-8
  (30 Hz) are not done either.

## Limits of the stand

- **Two server cores.** At 4,096 players the server needs about 1.5 cores of work: 0.5 for the tick and 1.0 for the
  gateway. Two cores leave no room to run them apart. A server with three or four cores would give the gateway two
  threads of its own: series 3 shows two threads deliver every snapshot, and with no shared core the tick would keep
  its 28 to 30 ms. That configuration could not be measured here. When the server takes three of machine A's cores, one is
  left for bots, about 2,500 of them, and machine B adds about 400.
- **The generator.** On machine A one core drives about 2,500 light bots (0.71 of a core; 3,300 saturated it) and
  about 1,700 default ones. Machine B is several times slower per bot: even with light bots it saturated at 800,
  so it carried 396 in series 3.
- **Latency.** Machine A's bots use loopback. Only machine B's bots crossed a real network, from the same data
  centre.

## Reproduce

Build `sawgrass-server` and `sawgrass-bots` in release mode on both machines, then follow
[Bots and server on separate machines](../../../crates/sawgrass-bots/README.md#bots-and-server-on-separate-machines).
Use the commands under [Machines and method](#machines-and-method) with the counts in the tables. Pin the server and
machine A's bots to separate cores with `taskset`, and give machine B's bots a different `--seed`. The full logs of
all three series are kept outside the repository; the tables above copy their "whole load phase" reports. `python3 -I chart.py`
in this folder draws the README's picture (`docs/assets/load.svg`) from the counted runs.
