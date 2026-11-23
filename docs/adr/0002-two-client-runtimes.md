# ADR 0002: Support two client runtimes, Leonida and XCL

Status: proposed, 2026-11-23

## Context

ADR 0001 put the client on top of [Leonida](https://github.com/ItsNotRexed/Leonida), a static recompilation of
the Xbox Series build. GTA VI does not boot under it yet. [XCL](https://github.com/ivall/xcl) is a second project
that may get there first: an Xbox Series X|S compatibility layer for Windows that runs a game's own x64 code
natively (no CPU emulation), reimplements the console system libraries and recompiles RDNA2 shaders. Its README
lists other titles as playable and does not mention GTA VI. Neither project has a plugin API for loading an outside
module.

The maintainers decided that in-game testing starts on whichever runtime first boots GTA VI to a controllable
character, so the client cannot be tied to one of them.

## Decision

- The client supports two **runtimes** behind one boundary. Everything above it (sync, prediction, UI, the
  `GameApi` trait and its users) is runtime-independent and is tested with the fake `GameApi`.
- A `Runtime` adapter per runtime does only what differs: how the module is loaded into the game process, how it
  hooks functions, and how game functions are located. Leonida recompiles the executable and XCL runs it as is, so
  function locations differ and patterns are kept per runtime and per game version, found at run time as before.
- The launcher detects which runtime the player has and checks that runtime's build against its own hash
  manifest.
- Stage 3 starts on the first runtime that boots the game to a controllable character. The second runtime gets its
  adapter later; nothing above the boundary changes for it.
- A plugin or module-loading API is proposed upstream to both projects rather than kept in a fork.
- This repository still contains nothing from the game, the console SDK or either runtime's binaries.

## Consequences

- `docs/architecture.md` describes the client in runtime-neutral terms and names both runtimes.
- Client tasks are split: runtime-independent work can start now against the fake `GameApi`; each runtime adapter
  is its own task and waits for that runtime.
- Open question for the maintainers: XCL is licensed GPL-2.0-only, which is not compatible with this project's
  AGPL-3.0 when both are combined in one program. Leonida is GPL-3.0, which is. Before an XCL adapter is written,
  the maintainers decide how the client module and XCL are combined (for example a separate process talking over
  IPC, or a differently licensed shim) or seek upstream relicensing.
