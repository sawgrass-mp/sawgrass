# ADR 0004: Replace Leonida with VIctory, keep XCL as the second runtime

Status: proposed, 2026-11-24. Supersedes ADR 0002 and the Leonida point of ADR 0001.

## Context

ADR 0001 put the client on Leonida, and ADR 0002 added XCL so that in-game testing could start on whichever booted
GTA VI first. Leonida's repository is no longer public, so it can no longer be a runtime.

[VIctory](https://github.com/Marvisak/VIctory) runs the Xbox Series X|S build of GTA VI natively on Windows: the
title's own x86-64 code runs in VIctory's process, VIctory implements the console libraries it imports, maps the
title's D3D12.X work onto host D3D12 and recompiles its shaders to DXIL on first use. Its README (status
2026-11-23) reports the game booting through the menus at 30 fps and loading the world, where most of the scene
does not render yet and frames take seconds. It is written in Rust, licensed GPL-3.0-or-later, keeps nothing from
the game in its repository, and is offline by design: the title cannot reach the network. It has no API for loading
an outside module, but it resolves every title import itself, can load DLLs the title asks for, and its
`victory-game` crate already holds per-build hooks for GTA VI.

[XCL](https://github.com/ivall/xcl) works the same way (console code run natively, system libraries reimplemented,
RDNA2 shaders recompiled). Its README lists other titles as playable and does not mention GTA VI. It is
GPL-2.0-only and has no plugin API either.

## Decision

- The client supports two **runtimes**, VIctory and XCL, behind the boundary from ADR 0002: a `Runtime` adapter per
  runtime is the only code that knows it (how the module is loaded, how hooks are installed, how game functions are
  found); everything above `GameApi` is runtime-independent and tested with the fake `GameApi`.
- Stage 3 starts on whichever runtime first runs the game to a world with a controllable character. VIctory is
  ahead today, so its adapter is expected first; the other follows, and nothing above the boundary changes for it.
- A small module-loading and hook API (load a listed library after the title starts, let it register hooks by
  build identity) is proposed upstream to both projects rather than kept in a fork. Until it lands, an adapter is
  developed against a local build of its runtime.
- The client module opens its own connection to the server. That is host networking from our code, not the
  title's, so it does not conflict with a runtime keeping the title offline; the upstream proposals say so.
- Function locations are found at run time per runtime and game build. This repository stores no bytes from the
  game executable, even where a runtime's own policy allows byte-pattern signatures.

## Consequences

- VIctory is GPL-3.0-or-later, which can be combined with this project's AGPL-3.0 (section 13 of each licence), so
  the VIctory adapter needs no relicensing.
- XCL is GPL-2.0-only, which cannot be combined with AGPL-3.0 in one program. The open question from ADR 0002
  stands: before an XCL adapter is written, the maintainers choose between a separate process talking to a small
  shim inside XCL over local IPC, licensing the in-process part so that it combines with GPL-2.0-only code (the
  relicensing proposed in OPS-8), or asking XCL's authors for a licence exception.
- The launcher detects the player's runtime, checks its build against that runtime's hash manifest, and never
  downloads the game or a runtime's converted data.
- `scripts/check_no_game_data.py` came from Leonida under GPL-3.0 and keeps that licence and attribution; Leonida
  being withdrawn does not change it.
