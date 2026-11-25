# Running a server for a playtest

How to run a Sawgrass server for the first sessions with the game: two or a few players who test that they see
each other (roadmap step 7's gate). It is not a guide for a public server: there are no accounts, no admin tools
and no roleplay yet, and the settings below favour learning over protection.

## What you need

- A machine the players can reach over the internet or a LAN, with one UDP port open. A dedicated server or a VPS
  is best; a laptop on Wi-Fi adds its own jitter to what you measure.
- This repository at the commit the players' client module was built from: the protocol version must match, and
  the server refuses a client that speaks another one.
- Rust, and libclang for the RocksDB build (on Debian or Ubuntu, `sudo apt install libclang-dev`).

## 1. Build

```sh
cargo build --release -p sawgrass-server -p sawgrass-bots
```

The server prints its protocol version on its first line; the client module and the bots of the same commit speak
the same one.

## 2. Certificate

Clients connect over QUIC and check the server's certificate against the one they were given. For a playtest a
self-signed certificate is enough:

```sh
target/release/sawgrass-bots gen-cert --out cert --name playtest.example
```

- `--name` is the name clients connect with (`server_name` in the client module's `ConnectConfig`). It does not
  have to resolve in DNS; it has to match.
- Give the players `cert/cert.der`, the server's address and port, and the name. **Never share `cert/key.der`**:
  whoever has it can pose as the server.

## 3. Open the port

QUIC runs over UDP only. Open the port you will listen on for UDP, for example with `ufw`:

```sh
sudo ufw allow 4433/udp
```

and in the hosting provider's firewall if it has one. TCP is not used.

## 4. Start

For the first session:

```sh
NAME=playtest-$(date +%Y%m%d-%H%M)
target/release/sawgrass-server --listen 0.0.0.0:4433 --cert cert/cert.der --key cert/key.der \
    --movement-report-only --list-players --record $NAME.rec | tee $NAME.log
```

- `--movement-report-only` counts every player state that breaks a movement rule but applies it. The game's map
  coordinates and speeds are not known yet, so a wrong limit would otherwise snap players back; with this flag the
  counts show which limit is wrong without spoiling the session. It covers the vehicles and NPCs a player drives or
  spawns too, so a car is not snapped back either. Leave it out once the limits are set.
- `--list-players` adds one line per connected player after each five-second line: its id, address, time
  connected, position and refused states. With it **the log holds the players' IP addresses**, so keep the log as
  private as the recording.
- `--record FILE` keeps everything the clients send, so the session can be played again through the bots (below).
- Keep the defaults for the rest. In particular do not use `--stats-to-clients` on a server players can reach.

If the counts show that the map or the speed limit is wrong, set them for the next session:

| Flag | Default | What it sets |
|---|---|---|
| `--map-min X,Y` | `-8192,-8192` | The map's south-west corner, in metres |
| `--map-size M` | `16384` | The map square's edge, in metres |
| `--height MIN,MAX` | `-500,3000` | Lowest and highest height allowed, in metres |
| `--max-speed M/S` | `150` | Fastest a player may move, on foot or in any vehicle |
| `--move-slack S` | `0.5` | Seconds of movement forgiven for network jitter |

## 5. Check it before the players join

From another machine, run two bots against the server for half a minute:

```sh
target/release/sawgrass-bots run --server SERVER_IP:4433 --cert cert.der --name playtest.example \
    --count 2 --duration 30 --area 100
```

The bots' `bots:` line should show about one entity per tick (each sees the other), and the server's line every
five seconds `players 2`. If the bots cannot connect, check the port (step 3) and the name (step 2).

## 6. During the session

The server prints a line every five seconds. Watch these parts of it:

- `players N` and the tick times after `tick`: two players should leave the tick far under its budget;
- `refused states N bounds N speed N teleport`: with `--movement-report-only` these are the states that would have
  been refused. Bounds counts mean the map square is wrong, speed counts mean the speed limit is too low, teleport
  counts are the game's own teleports (respawns, interiors) or a speed limit too low;
- `limits: dropped ... closed ...`: any closed connection here is worth a note with its time;
- `recording N written M dropped`: dropped records mean the disk was too slow, or more than 256 MiB was waiting to
  be written.

Write down the time of anything that looks wrong; the recording and the log can be read back at that time.

## 7. After the session

Stop the server with Ctrl+C. The recording is written out every half second, so at most that much is lost. Keep
the log and the recording together.

A recording holds everything the players sent: their positions, chat and script events, and with
`--list-players` the log holds their IP addresses. **Keep both private and never commit them to this repository.** To play it again against a server of the same version:

```sh
target/release/sawgrass-bots replay --file playtest-20261201-2000.rec --server 127.0.0.1:4433 \
    --cert cert/cert.der --name playtest.example
```

`--speed 0.25` plays it at a quarter of the speed, to look at a moment closely.
