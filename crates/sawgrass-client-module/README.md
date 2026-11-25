# sawgrass-client-module

The module that runs inside the game and connects it to a Sawgrass server (docs/roadmap.md, step 7). It is loaded
by a runtime that already runs the game, VIctory first and XCL later ([ADR 0004](../../docs/adr/0004-victory-client-runtime.md)),
through that runtime's own API. It has no loader or injector and works around nothing.

What exists now, all tested without the game:

- `Runtime`, the boundary each runtime adapter implements, and `GameBuild`, the build identity of the running game.
- `VictoryRuntime`, a stub that reports the missing upstream API.
- A pattern scanner and per-build function tables. Tables are text files kept outside this repository: no bytes
  from the game executable are ever committed here. A table names the build it was written for, and the module
  refuses to start on any other build, so each game patch needs its table checked and rewritten.
- `FakeRuntime`, over a made-up code image.
- `GameApi`, every change the module makes to the game, with `FakeGame`; the hook points (frame, creation and
  removal of peds, vehicles and objects, input, camera, world streaming) and the single-player systems switched off.
- Asset tables (protocol asset id to game asset, per build, outside the repository) and the entity pool (server id
  to game entity).
- `Module`: start (resolve, hook), frame (switch single player off, remove ambient peds and vehicles, hand events to
  the network core), stop. The network core implements `NetCore` and sees the game only through `World`.

Assumptions, to check against VIctory once it can load a module:

- The runtime reports the title's version and lets the module read the title's mapped code.
- The runtime installs a hook given a function address and calls the module's handler with the original available.
- Each single-player system can be switched off by one game function.
- The module is a Rust library with a C ABI (docs/architecture.md); the exported entry point waits for the loading API.
