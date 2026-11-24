# ADR 0005: Luau with native code generation for game-mode scripts

Status: accepted, 2026-11-24, by the maintainer's choice on the SCRIPT-1 measurements.

## Context

Game modes are resources written by server owners and modders (architecture.md, "Scripting"). The language was an
open decision in the roadmap: Luau (proposed), WebAssembly or JavaScript. The maintainers' priority is speed and
reliability together: scripts must be fast enough for the 50 ms tick at 4096 players, and a broken or hostile
script must not hang the tick or take the server down.

SCRIPT-1 measured nine engine setups on the same roleplay resource and five misbehaving scripts
([results](../research/scripting-engines/README.md), #78). On one core with the deadline check and the memory cap
installed:

| Engine | Zone check | Host call | Stops a loop and a long library call | Memory cap | Binary |
|---|---:|---:|---|---|---:|
| WebAssembly (wasmtime) | 0.18 µs | 5 ns | yes (no library to call) | exact | 11.3 MB |
| JavaScript, V8 | 0.25 µs | 23 ns | yes | soft, through an `unsafe` callback | 46.0 MB |
| LuaJIT | 0.30 µs | 36 ns | no | exact | 1.2 MB |
| **Luau, native code** | **1.59 µs** | **74 ns** | **yes** | **exact** | **3.0 MB** |
| Luau, interpreter | 3.87 µs | 93 ns | yes | exact | 2.2 MB |
| Lua 5.4 | 6.42 µs | 69 ns | no (library call) | exact | 0.9 MB |
| QuickJS-ng | 9.19 µs | 144 ns | yes | exact | 1.6 MB |
| Rhai | 68.97 µs | 279 ns | yes (no pattern matching) | none in total | 3.7 MB |

LuaJIT, Lua 5.4, QuickJS-ng and Rhai were ruled out by the data. Between Luau, V8 and WebAssembly the choice was
partly the language and partly what the client runtime allows, and the maintainer chose Luau.

## Decision

Server and client scripts are written in **Luau**, embedded through **`mlua` with native code generation**
(`luau-jit`) at optimization level 2.

- **Deadline:** every call into a resource runs under a deadline, enforced through Luau's interrupt callback
  (`Lua::set_interrupt`), which Luau also calls inside its pattern matcher. A stopped call returns an error to the
  server and the instance stays usable.
- **Memory:** each resource has a memory cap through mlua's allocator limit (`Lua::set_memory_limit`).
- **Sandbox:** resources run in Luau's sandbox (`Lua::sandbox`): read-only built-in libraries and globals, and no
  file, network or process access, on the server as on the client.
- **One instance per resource**, created when the resource starts; native code is compiled then (4.5 ms for the
  SCRIPT-1 resource), not in a tick.

## Consequences

- The scripting runtime is built on `mlua` (MIT) with vendored Luau (MIT), compiled from crates.io sources with a
  C++ compiler; it adds about 3 MB to the server.
- Luau does not check its interrupt inside `table.sort` with the default comparison, `string.rep` or
  `table.concat`. The memory cap bounds their input, but how long they can block was not measured; the scripting
  runtime measures that and guards them (a wrapper or a size limit) if it is too long.
- Script work done for every player on every tick is the expensive pattern (6.5 ms a tick for one zone check per
  player). The scripting API offers events from the server's own systems, such as zone entry from the spatial
  grid, instead of asking scripts to scan.
- Client scripts run in the same engine. If VIctory or XCL forbids executable memory, the client uses Luau's
  interpreter (2.4 times slower than native code on the zone check); that is checked when the client module
  starts (stage 3).
- Revisiting: if real resources spend most of their time per player per tick, V8 or WebAssembly would be faster by
  1.7 to 12 times on the SCRIPT-1 workloads; a later ADR can add WebAssembly beside Luau for heavy resources.
