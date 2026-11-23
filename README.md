# Sawgrass

An open-source multiplayer roleplay platform for Grand Theft Auto VI, written in Rust. The goal: **4096 players in
one world** on a dedicated server, with roleplay game modes written as sandboxed scripts.

> **Status: stage 0.** The repository rules, CI and task system are being set up. No playable build exists, and
> none can until the game boots on PC through [Leonida](https://github.com/ItsNotRexed/Leonida).

## How it fits together

- **Server** (`crates/sawgrass-server`): one world simulation process, several gateway processes that hold player
  connections over QUIC, PostgreSQL for persistent state.
- **Client module**: loaded by Leonida's runtime on the player's PC; syncs the local game with the server.
- **Launcher**: checks the player's Leonida build, installs the signed client module and starts the game.
- **Bots**: headless clients that speak the real protocol, so the server is built and load-tested at 4096 players
  without the game.

See [docs/architecture.md](docs/architecture.md) for the design and [docs/roadmap.md](docs/roadmap.md) for the
stages.

## This repository contains no game data

Nothing from the game is or will be in this repository: no archives, executables, assets or keys. You need your own
copy of the game. Nothing here decrypts or works around any protection.

## Contributing

The project is built by people and their AI coding agents. Read [CONTRIBUTING.md](CONTRIBUTING.md); agents start
with [AGENTS.md](AGENTS.md). Pick an issue labelled `ready`, comment `/claim`, and open a draft pull request.

## Licence

[GNU Affero General Public License v3.0](LICENSE). If you run a modified server for others, you must publish your
changes.

Grand Theft Auto and Grand Theft Auto VI are trademarks of Take-Two Interactive Software. This project is not
affiliated with or endorsed by Take-Two Interactive or Rockstar Games.
