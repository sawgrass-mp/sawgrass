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
| VIctory adapter | `victory.rs` | done against the mod loader's ABI 1 (a local VIctory branch, proposed upstream): loads, reads the tables, hooks, runs the frame; runs in the game on 1.0.2.1 (below); the build's `GameApi` and hook detours come next |
| Script natives | `victory/natives.rs`, `victory/script.rs` | calls a handler checked against its hash, with a script context current; the frame runs on a script thread and reports the local player's position; works in the game on 1.0.2.1; the `GameApi` over natives comes next |
| Functions located by pattern, per game build | `pattern.rs`, `table.rs` | done; tables live outside the repository |
| Hook points and their events, an event queue that hooks may push into from any thread | `hooks.rs` | done |
| `GameApi`: every change the module makes to the game, handles with generations | `game.rs`, `victory/game.rs` | trait and fake done; VIctory's over natives reads the local player and poses and sets a pose; everything not located yet is refused |
| Single-player systems switched off, ambient peds and vehicles removed | `module.rs` | done over `GameApi` |
| Protocol asset ids to game assets, per build | `assets.rs` | done; tables live outside the repository |
| The network core (`sawgrass-netcode`) in the module: remote entities shown, the local player sent and corrected, owned entities left to the game and their states sent | `link.rs` | done; ped and vehicle blocks are not read from the game yet |
| The local player's shots reported, and the weapon in hand sent with its state | `module.rs`, `link.rs` | done over `GameApi` and the `weapon_fire` hook |
| The module's own connection to a server | `connection.rs` | done; it also sends the link's client events |
| The server's clock, weather and doors applied to the game | `world_state.rs` | done over `GameApi` |
| The local player's seat: asked of the server, seated on its answer | `module.rs`, `link.rs` | done over `GameApi` and the seat hooks; remote peds' seats wait for the blocks task |

`tests/two_clients.rs` is the first milestone without the game: two modules, each in its own fake game, connect to
the real server over loopback and each shows the other player's ped where that player stands.

## What the first day with the game needs

None of this can be in the repository; it comes from a maintainer with the game:

1. A VIctory build that loads the module and installs hooks: the upstream API from ADR 0004, or a local build of
   VIctory until it lands. `VictoryRuntime` is written against the local mod loader's ABI 1; see "Running under
   VIctory" below.
2. A function table for the exact game build. Two entries are enough to start, `module::REQUIRED_FUNCTIONS`:
   `frame` (hooked once a frame) and `local_player` (where the local player's ped is kept, or the native that
   names it). A runtime that calls the module once a frame by itself, as VIctory's loader does, makes `frame`
   unneeded. Two players who see each other also need `ped_spawn` (creates a ped) and `entity_set_pose` (puts an
   entity at a position), but while those are still being found the module starts without them: it sends the
   local player, shows no remote peds, moves no entity, and says so in the log (`Module::shows_remote_peds`,
   `Module::moves_entities`). The module refuses to start without the required ones, and refuses a
   table for another build. Every other entry (`module::optional_functions()`: creation and removal of peds,
   vehicles and objects, input, camera, streaming, weapon fire, seat enter and leave, the five single-player
   switches) is used when it is there and switches its feature off when it is not, as does a hook the runtime
   refuses; `Module::missing_functions` and `Module::refused_hooks` list what is still to find.
3. An asset table for that build, with at least the `ped` line (the model players are shown with) and one vehicle,
   plus each weapon the server's resources allow (a shot with a weapon missing from it is not reported) and a line
   for each weather and each door they name (ADR 0006, assumption A4).
4. The game's coordinate conventions, checked once: `link.rs` assumes a Z-up world with the heading as yaw about Z.
5. A server both clients can reach, and its certificate for `ConnectConfig`.

Then the real `GameApi` and the hooks' argument reading are written per build, and the milestone is two players who
see each other in the game.

## Running under VIctory

Build the module (`cargo build --release -p sawgrass-client-module` on Windows gives `sawgrass_client_module.dll`;
the Windows build is not checked by CI yet) and start the game with three variables set:

- `VICTORY_MODS`: the path of that DLL, for VIctory's mod loader.
- `SAWGRASS_FUNCTION_TABLE` and `SAWGRASS_ASSET_TABLE`: the paths of the two tables on this machine.

For a run nobody attends, the local VIctory branch also reads `VICTORY_PAD_START_A=1`, which gets the game past its
start screen without a controller press. That is VIctory's setting, not the module's.

State in the game, on 1.0.2.1: the module loads, checks `local_player` and `entity_position` as natives, starts
with only those located, runs its frame on a script thread, and logs the local player's position every 600 frames,
with no crash. `entity_set_pose` and `ped_spawn` are not located yet, so the log warns that no entity is moved
and remote players are not shown.

To join a server, set three more variables:

- `SAWGRASS_SERVER`: the server's `address:port` (the server's `--listen`).
- `SAWGRASS_SERVER_CERT`: the server's certificate (DER) the connection must chain to; for a test server,
  `sawgrass-bots gen-cert --out DIR` writes `cert.der` and `key.der`, which the server takes as `--cert` and
  `--key`.
- `SAWGRASS_SERVER_NAME`: the name in that certificate, `localhost` if unset.

With them the module opens its connection at the start (`sawgrass: connecting to ...`), and on every script-thread
frame runs `Module::frame` with the natives' `GameApi` (`victory/game.rs`) and the network core, so the server
gets the local player. The log shows each change of the connection (`sawgrass: connected`,
`sawgrass: connection closed: ...`), and the report every 600 frames adds `joined as entity N, K shown, M not shown
yet`. The network core needs the script thread: when the frame stays at the present, the log says the module is
not connecting. Without `SAWGRASS_SERVER` the module runs on its own, as before.

VIctory's log then shows `sawgrass: running build victory/<version>/<hash>` (the key for the tables' `build`
lines), `sawgrass: started, N functions located`, one `off until found` line per optional function the table does
not locate yet, and every 600 frames `sawgrass: N frames` (or, on a script thread, the line described below). A table for another build, or a required entry missing,
shows `sawgrass: not started` with the reason, and nothing is hooked. Take patterns from the running process, not
from the executable file: VIctory rewrites code before the module sees it.

### Script natives

The game layer will mostly call the title's script natives rather than its internal functions: a native is a
handler the title's own scripts call, so it keeps its meaning across builds. `victory/natives.rs` calls them the
way the title does, on a script thread with that thread's execution context current in the title's TLS (through
the host's `title_tls`, added to ABI 1 at host size 104; a warning in the log says when the VIctory in use lacks
it). Before a handler is used it is checked against its native's hash, without running it, so a wrong address in
the table is refused instead of called. How the check reads a handler is itself in the table: guard patterns
(`native_guard`, `native_guard_<name>`) with the relative reference each leads through, the windows to search,
and the rotation and mask that turn a hash into the identity the code must hold. A guard pattern has a fixed
length, so a table lists one line per shape it meets, for example one with an instruction of each length between
the compare and the branch, written as wildcards; the identity, a full 64-bit value, rejects a false match. A
refused native's log line says so (`native local_player refused: ...`), and the fix is a guard line in the table,
not a change here. The repository has only the
generic matcher. Handler addresses, hashes, the check and the script-thread offsets all come from the table on
the player's machine; none are in this repository.

The module's frame runs on a script thread (`victory/script.rs`) when the table has these entries:

- `script_run`: the title's script run function, hooked;
- `value script_tls_context`: the current context's offset in the title's TLS block;
- `value script_context_thread`: where a context points to its thread;
- `value script_thread_state`: where a thread keeps its state byte;
- `value script_thread_updating`: a bit mask of the states in which natives may run.

The frame then runs once per presented frame, right after the first script run on a thread the title is
updating. The log says `sawgrass: frame runs on a script thread`, or `frame stays at the present` with the
reason, and every 600 frames `sawgrass: N frames on a script thread`. When `local_player` and `entity_position`
are natives in the table (`; hash=`) and pass the check, that line adds the local player's position, which shows
end to end that native calls work.

The game layer for VIctory (`victory/game.rs`, `NativeGame`) is a `GameApi` over the natives the table locates:
`local_player`, `entity_position`, `entity_heading` (0 without it) and `entity_set_pose`. Each one that is checked
is logged as `native <name> checked`; whatever is not located yet answers `Refused` or `None`, so the module runs
with what is found.

## Table formats

Function table:

```text
build = victory/1.0.2.1/0123abcd
ped_create = 0A ?? 0C 0D
vehicle_create = 1A 1B ?? ?? ?? ?? ; offset=2 ; rel32=1,5
local_player = @0x2A41F80      # an offset from the image base, as a debugger shows it
entity_set_pose = @0x1B2C3D0 ; hash=0x0123456789ABCDEF   # a script native's handler, with its hash
native_guard = AA BB ?? CC ; rel8=2,4                    # how handlers are checked (see Script natives)
value native_check_window = 0x100                         # a number that is not an address
```

Modifiers: `offset=N` moves the match, `rel32=D,E` and `rel8=D,E` follow a relative reference (the displacement
D bytes in, counted from the instruction's end E bytes in), and `hash=H` marks a native's handler.

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
- The enter-seat hook can keep the game from seating the local player, so that the server decides; the module
  then seats it with `GameApi::enter_seat` on the server's grant. The detour must let that call through. Only one
  seat request waits for the server at a time, so a game that calls the entry function every frame does not flood
  the server.
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
