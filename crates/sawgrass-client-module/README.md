# sawgrass-client-module

The module that runs inside the game and connects it to a Sawgrass server (docs/roadmap.md, steps 7 and 8). It is
loaded by a runtime that already runs the game, VIctory first and XCL later
([ADR 0004](../../docs/adr/0004-victory-client-runtime.md)), through that runtime's own API. It has no loader or
injector and works around nothing.

## What works without the game

Everything below is tested on `FakeRuntime` and `FakeGame`:

| Part | Where | State |
|---|---|---|
| Runtime boundary, build identity | `runtime.rs` | done |
| VIctory adapter | `victory.rs` | stub: reports the missing upstream module-loading and hook API |
| Functions located by pattern, per game build | `pattern.rs`, `table.rs` | done; tables live outside the repository |
| Hook points and their events, an event queue that hooks may push into from any thread | `hooks.rs` | done |
| `GameApi`: every change the module makes to the game, handles with generations | `game.rs` | trait and fake done; the real one waits for the game |
| Single-player systems switched off, ambient peds and vehicles removed | `module.rs` | done over `GameApi` |
| Protocol asset ids to game assets, per build | `assets.rs` | done; tables live outside the repository |
| The network core (`sawgrass-netcode`) in the module: remote entities shown, the local player sent and corrected, owned entities left to the game and their states sent | `link.rs` | done; ped and vehicle blocks are not read from the game yet |
| The local player's shots reported, and the weapon in hand sent with its state | `module.rs`, `link.rs` | done over `GameApi` and the `weapon_fire` hook |
| The module's own connection to a server | `connection.rs` | done; it also sends the link's client events |
| The server's clock, weather and doors applied to the game | `world_state.rs` | done over `GameApi` |

`tests/two_clients.rs` is the first milestone without the game: two modules, each in its own fake game, connect to
the real server over loopback and each shows the other player's ped where that player stands.

## What the first day with the game needs

None of this can be in the repository; it comes from a maintainer with the game:

1. A VIctory build that loads the module and installs hooks: the upstream API from ADR 0004, or a local build of
   VIctory until it lands. `VictoryRuntime` is written against it.
2. A function table for the exact game build. Four entries are enough to start, `module::REQUIRED_FUNCTIONS`:
   `frame` (hooked once a frame), `local_player` (where the local player's ped is kept), `ped_spawn` (creates a
   ped) and `entity_set_pose` (puts an entity at a position). A runtime that calls the module once a frame by
   itself, as VIctory's loader does, makes `frame` unneeded. The module refuses to start without them, and refuses a
   table for another build. Every other entry (`module::optional_functions()`: creation and removal of
   peds, vehicles and objects, input, camera, streaming, weapon fire, the five single-player switches) is used when it is there
   and switches its feature off when it is not, as does a hook the runtime refuses; `Module::missing_functions`
   and `Module::refused_hooks` list what is still to find.
3. An asset table for that build, with at least the `ped` line (the model players are shown with) and one vehicle,
   plus each weapon the server's resources allow (a shot with a weapon missing from it is not reported) and a line
   for each weather and each door they name (ADR 0006, assumption A4).
4. The game's coordinate conventions, checked once: `link.rs` assumes a Z-up world with the heading as yaw about Z.
5. A server both clients can reach, and its certificate for `ConnectConfig`.

Then the real `GameApi` and the hooks' argument reading are written per build, and the milestone is two players who
see each other in the game.

## Table formats

Function table:

```text
build = victory/1.0.2.1/0123abcd
ped_create = 0A ?? 0C 0D
vehicle_create = 1A 1B ?? ?? ?? ?? ; offset=2 ; rel32=1,5
local_player = @0x2A41F80      # an offset from the image base, as a debugger shows it
```

An `@` entry holds only for the build on the `build` line; a pattern usually survives a small patch, so an offset
found first is best replaced by a pattern once one is known.

Asset table:

```text
build = victory/1.0.0/0123abcd
ped = 0x2000      # the ped model for players and peds with no model
17 = 0x1234       # protocol asset id = the game's value for it
```

## Assumptions to check against VIctory

- The runtime reports the title's version and lets the module read the title's mapped code.
- The runtime installs a hook given a function address and calls the module's handler with the original available.
- The game has no generation for its entity handles; the adapter counts them (`game::Generations`).
- Each single-player system is switched off by one game function.
- One game function resolves a pull of the trigger with all of its hits, a shotgun's pellets included.
- Until the game's ped and vehicle state is read, the player is sent as a standing ped holding the weapon
  `GameApi::held_weapon` reports, and an owned entity's block is sent back as the server last sent it. The server
  keeps health, armour, seats and damage itself (ADR 0006), so only the weapon and the kinematics of owned entities
  change on the server.
- The game keeps a door's state whether or not the door is streamed in, and lets the module hold its clock still
  between the frames that set it.
- Weather and door ids share the asset table with models, as opaque protocol ids (ADR 0006, section 2).
- The module is a Rust library with a C ABI (docs/architecture.md); the exported entry point waits for the loading
  API.
