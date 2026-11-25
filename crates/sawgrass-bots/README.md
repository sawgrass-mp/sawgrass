# sawgrass-bots

Headless load-test bots. Each bot connects to a Sawgrass gateway over QUIC exactly as a game client does (same
handshake, same packets), walks, runs or drives between waypoints, sends its state 20 times a second and reads the
snapshots the server sends back. Waypoints are spread over a square, with a share of them in a few hotspots, so
crowds form and break up as they do on a roleplay server.

## Load report for N bots

Build both binaries in release mode and write a test certificate (self-signed, for tests only):

```sh
cargo build --release -p sawgrass-server -p sawgrass-bots
target/release/sawgrass-bots gen-cert --out /tmp/sawgrass-cert
```

Start the server with `--stats-to-clients`, so it sends its tick and CPU figures to the bots:

```sh
target/release/sawgrass-server --listen 0.0.0.0:4433 \
    --cert /tmp/sawgrass-cert/cert.der --key /tmp/sawgrass-cert/key.der --stats-to-clients
```

Then run 100 or 1000 bots:

```sh
target/release/sawgrass-bots run --server 127.0.0.1:4433 --cert /tmp/sawgrass-cert/cert.der --count 100
target/release/sawgrass-bots run --server 127.0.0.1:4433 --cert /tmp/sawgrass-cert/cert.der --count 1000
```

Bots connect at `--connect-per-sec` (default 200), then load the server for `--duration` seconds (default 60).
A report is printed every `--report-every` seconds (default 10) and once for the whole load phase:

```text
bots 1000/1000 connected (0 failed, 0 dropped), 0 connect retries, period 30 s ending at 35 s
server: 1000 players, tick p50 4.09 ms, p99 8.22 ms (16.4% of 50 ms, budget 70%), max 14.95 ms, 0 overruns
server: cpu 0.64 cores (tick thread 0.09, network the rest), down 8.94 kB/s and up 0.52 kB/s per player
bots: down 9.31 kB/s per bot in 22.3 datagrams/s, 20.0 ticks/s with a snapshot, 19.9 entities per tick; up 0.52 kB/s per bot
generator: cpu 0.85 of 2 cores, send lateness p50 <2.05 ms p99 <4.10 ms (period 50 ms): ok
```

- **server** lines come from the server itself: tick percentiles over its metrics window (the last minute), and
  CPU and traffic over its last five seconds. Bytes are datagram bytes with packet headers, without QUIC and UDP
  overhead.
- **bots** lines are measured by the bots: what arrived, how many server ticks each bot got a snapshot for (20
  per second at 20 Hz means nothing was lost), and how many entities each snapshot carried.
- **generator** says whether the bots themselves kept up. It reads `SATURATED` when the bots used more than 85%
  of their machine's cores, or when their state sends ran later than one send period at p99. A saturated run
  measures the load generator, not the server: run the bots on another machine, or on more of them.

Other options: `--send-hz` (default 20), `--area` (side of the square in metres, default 4000), `--hotspots`
(default 8), `--seed` (same seed, same routes), `--threads` (runtime threads, default all cores) and `--name`
(the name in the server certificate, default `localhost`).

A bot the server refuses, or whose handshake times out, tries again after a pause drawn between half and all of
250 ms, doubling up to 4 s, until the run ends, as a game client does when a server is busy (BOT-6). The first line
counts those retries; a bot counts as failed only if it never got in.

### A lighter generator

The bots take more CPU than the server's gateway, so one machine of the same size cannot always load a full
server. Two options make each bot cheaper without changing what the server sees:

- `--decoders N`: only bots numbered below N decode their snapshots. The others reassemble each tick, check that
  its baseline is a tick they acknowledged, and acknowledge it without decoding the entities. The server gets the
  same acknowledgements, so it builds the same deltas and sends the same bytes. The entities-per-tick figure comes
  from the decoders, so keep a few dozen of them.
- `--sockets N`: the bots share N UDP sockets, each bot keeping its own QUIC connection, keys and streams, instead
  of one socket each. The server's work per connection is the same, and its per-address limits count IP
  addresses, which bots on one machine share either way.

At 3,000 bots on 2 cores over 1,500 m, one socket per bot with every bot decoding was SATURATED at 1.84 cores
(state sends late by up to 131 ms at p99), while `--sockets 32 --decoders 32` stayed ok at 1.5 cores (p99 under
8 ms). 4,096 bots in that mode stayed ok at 1.64 cores. Use the default for figures that depend on what bots
receive, and the light mode to load a server with more bots than the generator could otherwise carry.

### Bots that pay each other

`--pay-every SECS` makes every bot pay another bot of the run 1 to 100 units every SECS on average (BOT-7), against a
server started with `--ledger DIR --money-events` (DB-8). Each payment is answered once it is on disk, and the report
adds a line:

```text
money: 1998 payments/s sent, 1994/s done, 0 short of funds, 0 refused otherwise; answer p50 <16.38 ms p99 <524.29 ms
```

"done" is the payments the server confirmed. "short of funds" means the payer had too little; "refused otherwise"
covers any other answer. The answer time runs from the send to the confirmation, the ledger's group commit included.

`--record FILE` writes every snapshot datagram the first `--record-bots` bots (default 32) receive to FILE, for the
packet compression benchmark to replay (`docs/research/packet-compression`). It takes about 70 MB a minute at 1,000
bots over 1,500 m.

`--misbehave flood=N,teleport=N,bad-event=N,reconnect=N` adds misbehaving bots next to the `--count` honest ones, to
test the server's defences under load (BOT-4). A `flood` bot sends 1,000 states a second, and a `teleport` bot jumps
1 km a tick at velocity 0. A `bad-event` bot sends an event the server does not know, and a `reconnect` bot connects
and closes five times a second. Flood and bad-event bots reconnect when the server closes them. Their counters stay
out of the honest figures and get one line each at the end of the report:

```text
misbehaving flood: 20 bots, 400 connects, 0 failed, 380 closed by the server, 1180000 sent
```

The server refuses connections beyond 16 from one address by default, so run it with `--max-per-address` above the
total number of bots when they all come from one machine.

## Bots and server on separate machines

The bots reach the server over the network like players, so give them the server's address and the name in its
certificate:

```sh
# On the server machine:
target/release/sawgrass-bots gen-cert --out cert --name server.lan
target/release/sawgrass-server --listen 0.0.0.0:4433 --cert cert/cert.der --key cert/key.der --stats-to-clients
# Copy cert/cert.der (not the key) to the bot machine, then:
target/release/sawgrass-bots run --server 192.0.2.10:4433 --name server.lan --cert cert.der --count 4096
```

Several bot machines can load one server at once; give each a different `--seed`. Only the bot that asks first
needs the stats, but every machine prints the server lines.

Each bot has its own UDP socket, like a real client, unless `--sockets` is given. For thousands of bots raise the open-file limit first
(`ulimit -n 10000`).

On one machine, pin the two to separate cores so they do not take CPU from each other (Linux):

```sh
taskset -c 0,1 target/release/sawgrass-server ... --net-threads 1
taskset -c 2,3 target/release/sawgrass-bots run ...
```

## What this does not measure yet

- Snapshots are whole: every visible entity every tick. Delta encoding, chosen in
  `docs/research/packet-compression`, will cut the bytes per player once the protocol has it.
- The gateway runs inside the server process; the `network the rest` CPU figure is the gateway's share until
  gateways move to their own processes.
- Bots only move. They do not yet send roleplay events, so the report says nothing about the storage write load.
