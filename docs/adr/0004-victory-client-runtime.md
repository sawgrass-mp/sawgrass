# ADR 0004: Target VIctory as the client runtime

Status: proposed, 2026-11-24. Supersedes ADR 0002 and the Leonida point of ADR 0001.

## Context

ADR 0001 put the client on Leonida, and ADR 0002 added XCL so that in-game testing could start on whichever booted
GTA VI first. Leonida's repository is no longer public. XCL does not list GTA VI among the titles it runs.

[VIctory](https://github.com/Marvisak/VIctory) runs the Xbox Series X|S build of GTA VI natively on Windows: the
title's own x86-64 code runs in VIctory's process, VIctory implements the console libraries it imports, maps the
title's D3D12.X work onto host D3D12 and recompiles its shaders to DXIL on first use. Its README (status
2026-11-23) reports the game booting through the menus at 30 fps and loading the world, where most of the scene
does not render yet and frames take seconds. It is written in Rust, licensed GPL-3.0-or-later, keeps nothing from
the game in its repository, and is offline by design: the title cannot reach the network.

VIctory has no API for loading an outside module. It does resolve every title import itself, can load DLLs the
title asks for, and its `victory-game` crate already holds per-build hooks for GTA VI.

## Decision

- The client targets **VIctory** only. In-game testing (Stage 3) starts when the game reaches a world with a
  controllable character under it.
- The boundary from ADR 0002 stays: a `Runtime` adapter is the only code that knows VIctory (how the module is
  loaded, how hooks are installed, how game functions are found); everything above `GameApi` is runtime-independent
  and tested with the fake `GameApi`. Another runtime would need only a new adapter and its own ADR.
- We propose a small module-loading and hook API to VIctory upstream (load a listed library after the title
  starts, and let it register hooks by build identity) rather than keep a fork. Until it lands, the adapter is
  developed against a local VIctory build.
- The client module opens its own connection to the server. That is host networking from our code, not the
  title's, so it does not conflict with VIctory keeping the title offline; the upstream proposal says so.
- Function locations are found at run time per game build. As before, this repository stores no bytes from the
  game executable, even though VIctory's own policy allows byte-pattern signatures.

## Consequences

- Licensing: GPL-3.0-or-later can be combined with this project's AGPL-3.0 (section 13 of each licence), so a
  client module loaded into VIctory needs no relicensing. The distributed combination is under the GPL-3.0 terms
  for VIctory's part and AGPL-3.0 for ours. The XCL licensing question in ADR 0002 no longer applies.
- The launcher checks the player's VIctory build against a hash manifest and never downloads the game or VIctory's
  converted data.
- `scripts/check_no_game_data.py` came from Leonida under GPL-3.0 and keeps that licence and attribution; Leonida
  being withdrawn does not change it.
