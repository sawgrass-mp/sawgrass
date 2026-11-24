# The real server under the load-test bots

Measured 2026-11-24 with `sawgrass-server` and `sawgrass-bots` from `main` (BOT-2, #76) on a 4-vCPU, 15 GB VM,
server and bots on the same machine. Task: BOT-3 (#87).

The earlier comparisons ([transport](../network-transport/README.md), [serialization](../packet-serialization/README.md),
[compression](../packet-compression/README.md), [visibility](../visibility-index/README.md)) were measured on
benchmark harnesses: synthetic clients, synthetic traffic, the server's code taken out of the server. The bots
connect to the real server through its QUIC gateway as game clients do, walk, run and drive between waypoints, and
gather in hotspots. This page records what the real server does under them, and which of the earlier conclusions
the figures confirm. Each report it checks has a section "Checked on the load-test bots" that links back here.

## Short answer

- **The transport choice holds.** quinn in the real server spent 0.33 to 0.97 cores on its gateway thread for
  1,000 to 3,000 players, which is 4.7 to 9.4 µs per datagram, at or below the 7.9 to 12.8 µs of the NET-3
  harness. See [Transport](#transport-quinn-in-the-real-server).
- **The real limit on this VM is the tick thread, not the network.** In a city-density crowd (1,500 m square,
  about 150 players in view) the server overran its 50 ms tick at 2,000 players (p99 47 to 54 ms) and fell to
  12 ticks a second at 3,000, dropping 6 to 8% of the bots. Spread over 4 km it held 3,000 at p99 28 ms.
- **Where the tick goes**: in a profile of the tick thread at 2,000 players in a crowd, visible sets took about a
  quarter, memory allocation about a quarter and waking the gateway about a fifth. The dense grid chosen in IM-2
  speeds up the first part 1.8 to 2.8 times on the bots' movement, but that is not enough on its own.
- **A faster allocator is the cheapest gain measured.** With jemalloc or mimalloc in place of glibc's malloc the
  same crowd of 2,000 ran at tick p50 17.5 ms and p99 26 to 27 ms instead of 30.5 and 52 ms, with 0 to 2 overruns
  instead of 5 to 54. This confirms, and makes larger, the jemalloc choice of SRV-5 (#84, not yet merged), whose
  harness saw 6% on the tick. See [Allocators](#allocators).
- **Not measured on the bots**: storage, scripting and voice (the server does not run them yet and the bots send
  no such load), the tick model (SRV-4, #81, is not merged), and every transport other than quinn (the bots and
  the gateway speak only QUIC).

**4,096 players were not measured.** At 3,000 bots the generator used 1.4 to 1.6 of its 2 cores, short of its
85% limit; beyond that it would saturate first. The server's limit for 4,096 players needs the bots on a second
machine ([bots README](../../../crates/sawgrass-bots/README.md#bots-and-server-on-separate-machines)).

## Method

[`run.py`](run.py) starts `target/release/sawgrass-server` pinned to CPUs 0 and 1 with one gateway thread next to
its tick thread, and `target/release/sawgrass-bots` pinned to CPUs 2 and 3 with two runtime threads, as the bots
README advises for one machine. Each run connects N bots at 200 a second, loads the server for 60 s, and records the
bots' report for the whole load phase: tick percentiles and CPU from the server itself, traffic and snapshots as the
bots received them, and whether the generator kept up. Every run is repeated three times with seeds 1, 2 and 3
(different hotspots and routes).

Two densities, both with the bots' 8 hotspots drawing 30% of waypoints:

- **4 km**: bots spread over a 4 km square, as the bots default; 15 to 50 players in view.
- **1.5 km**: the same bots over a 1.5 km square, a dense city; 70 to 190 players in view.

None of the runs below had a saturated generator. The tick thread's share is the server's own figure; "network" is
the rest of the server process, that is the gateway's tokio thread and quinn.

## Results

Means of three runs (min to max where it matters); all figures in [`results/transport.jsonl`](results/transport.jsonl).

| Bots | Square | In view | Tick p50 / p99 | Overruns | Server CPU | Tick thread | Network | Down per player | Datagrams per player |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 4 km | 17 | 3.5 / 6.0 ms | 0 | 0.45 | 0.07 | 0.38 | 7.0 kB/s | 21 /s |
| 1,000 | 1.5 km | 75 | 8.7 / 13.8 ms | 0 | 0.66 | 0.17 | 0.49 | 35.8 kB/s | 41 /s |
| 2,000 | 4 km | 32 | 8.8 / 13.8 ms | 0 | 0.76 | 0.14 | 0.62 | 13.4 kB/s | 26 /s |
| 2,000 | 1.5 km | 148 | 30.2 / 50.6 ms | 6 to 34 | 1.43 | 0.57 | 0.86 | 71.2 kB/s | 72 /s |
| 3,000 | 4 km | 48 | 16.8 / 26.8 ms | 0 to 1 | 1.24 | 0.29 | 0.95 | 20.0 kB/s | 31 /s |
| 3,000 | 1.5 km | 188 | 58.9 / 114 ms | 595 to 820 | 1.59 | 0.81 | 0.78 | 70.8 kB/s | 55 /s |

At 3,000 bots in the dense square the server is past its limit: 12.3 ticks a second reached the bots instead of
20, and 174 to 232 bots were dropped. The tick thread was busy 81% of the time and the gateway thread most of the
rest of the two cores. The generator was at 1.0 to 1.1 of its 2 cores, so this is the server, not the bots.

### Transport: quinn in the real server

The NET-3 harness loaded a quinn server with 2 snapshot datagrams of 1,000 bytes per client per tick and a client
state each tick, 60 datagrams per player per second. The bots in the 1.5 km square produce nearly that (41 down and
20 up at 1,000 bots, 72 and 20 at 2,000). Per datagram through the gateway:

| | Players | Datagrams per second | Network CPU | Per datagram |
|---|---:|---:|---:|---:|
| NET-3 harness, quinn | 1,000 | 60,000 | 0.77 cores | 12.8 µs |
| NET-3 harness, quinn | 4,096 | 245,760 | 1.94 cores | 7.9 µs |
| Real server, 4 km | 1,000 | 40,600 | 0.38 cores | 9.4 µs |
| Real server, 1.5 km | 1,000 | 61,500 | 0.49 cores | 8.0 µs |
| Real server, 1.5 km | 2,000 | 184,000 | 0.86 cores | 4.7 µs |
| Real server, 4 km | 3,000 | 153,000 | 0.95 cores | 6.2 µs |

The real gateway costs no more per datagram than the harness did, so the NET-3 estimate of 0.8 to 1 core for a
gateway of 1,000 to 1,400 players is, if anything, high. Two differences make the real figures lower: the harness
also echoed a reliable event per player each second, and the real snapshot datagrams are smaller (860 to 990 bytes on average in the dense square).
The figures are on a loopback link with no loss; NET-3's loss measurements are not repeated here.

### Where the tick goes

`perf record -e cpu-clock` on the tick thread, 12 s at 2,000 bots in the 1.5 km square (tick p50 31.6 ms):

| Part of the tick | Share of the tick thread |
|---|---:|
| Visible sets (`VisibleSet::update`, its sort, the grid's hashing) | 26% |
| Memory allocation (`malloc`, `realloc`, `free` and glibc's housekeeping) | 27% |
| Waking the gateway (kernel time in `futex`, lock release, task switches) | 22% |
| Gathering snapshot states (`send_snapshots` copying states out of the hash map) | 8% |
| Encoding snapshots and handing datagrams to quinn | 6% |
| The rest | 11% |

The allocations are the per-datagram `Vec` and `Bytes` of each snapshot and the visible sets' sorted lists. The
wake-ups come from calling quinn's `send_datagram` on the tick thread for each of about 7,000 datagrams a tick:
each one may wake the connection's task on the gateway thread.

## Allocators

The same 2,000 bots, three runs each, with the server's `malloc` replaced through `LD_PRELOAD`
([`results/allocator.jsonl`](results/allocator.jsonl)). glibc is the six runs of the server as built: the three of the
transport suite and three more run right after the other allocators, to rule out drift between the hours.

| Allocator | Square | Tick p50 | Tick p99 (min to max) | Overruns per run | Server CPU | Tick thread | Network |
|---|---:|---:|---:|---|---:|---:|---:|
| glibc 2.39 | 4 km | 8.8 ms | 13.8 ms (13.4 to 14.6) | 0, 0, 0 | 0.76 | 0.14 | 0.62 |
| jemalloc 5.3.0 | 4 km | 5.6 ms | 10.8 ms (9.5 to 12.7) | 0, 1, 0 | 0.72 | 0.12 | 0.60 |
| mimalloc 2.1.2 | 4 km | 6.0 ms | 10.1 ms (9.5 to 10.4) | 0, 0, 0 | 0.67 | 0.12 | 0.56 |
| glibc 2.39 | 1.5 km | 30.5 ms | 52.1 ms (43.3 to 59.0) | 34, 15, 6, 54, 5, 47 | 1.42 | 0.58 | 0.84 |
| jemalloc 5.3.0 | 1.5 km | 17.5 ms | 26.4 ms (25.1 to 27.3) | 0, 2, 0 | 1.15 | 0.37 | 0.78 |
| mimalloc 2.1.2 | 1.5 km | 17.6 ms | 27.1 ms (25.8 to 27.7) | 2, 0, 1 | 1.17 | 0.38 | 0.80 |

In the crowd, either allocator took 13 ms off the tick's median and halved its p99, bringing the server from over
its 50 ms tick back to 53% of it. The tick thread's CPU fell from 0.58 to 0.37 cores: glibc's `malloc` and its
housekeeping were the quarter of the tick seen in the profile. The gateway thread gained little (0.84 to 0.78 cores).
jemalloc and mimalloc are within noise of each other.

**SRV-5 (#84) chose jemalloc** on its own harness, where it lowered the tick p50 by 6%. On the real server the gain
is far larger, because the real tick allocates per datagram and per visible set, which that harness did not, so the
choice is confirmed. These are the distribution's builds, not the `tikv-jemallocator` and `mimalloc` crates SRV-5
measured; jemalloc is the same version (5.3.0). Memory use was not measured here.

## What these runs do not show

- **4,096 players**, as above: the generator's limit on one machine comes first.
- **A real network**: loopback, no loss, no jitter. Loss and latency stay as NET-3 measured them.
- **A long run**: 60 s of load each. Memory growth over hours is not measured.
- **Choices not yet in the server**: postcard, delta snapshots and the dense grid are not in `sawgrass-server`
  yet, so the server here runs the hand-written format, whole snapshots and the IM-1 grid. Their own sections say
  how each was checked instead.

## Reproduce

```sh
cargo build --release -p sawgrass-server -p sawgrass-bots     # at the workspace root
cd docs/research/load-test-bots
python3 -I run.py transport --counts 1000,2000,3000 --areas 4000,1500 --repeats 3   # about 25 minutes
python3 -I run.py allocator --counts 2000 --areas 4000,1500 --repeats 3 \
    --preload /usr/lib/x86_64-linux-gnu/libjemalloc.so.2 --label jemalloc           # about 8 minutes
```

The allocator runs use the distribution's shared libraries (Ubuntu 24.04: `libjemalloc2` 5.3.0, `libmimalloc2.0`
2.1.2) through `LD_PRELOAD`, which replaces `malloc` for the whole server process without changing a crate. Raise
the open-file limit if `run.py` cannot (`ulimit -n 20000`).
