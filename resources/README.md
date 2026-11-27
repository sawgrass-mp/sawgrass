# Resources

Roleplay game modes live here, one folder per resource. A resource uses only the public scripting API; it never
reaches into server internals. CI will enforce that boundary once the scripting runtime exists (stage 2), so that
this folder can later move to its own repository without changes.

## Writing a resource

Resources are written in [Luau](https://luau.org) ([ADR 0005](../docs/adr/0005-luau-scripting.md)). `hello/` is the
smallest one.

```text
hello/
  resource.toml        server = ["server/main.luau"]
  server/main.luau
```

`resource.toml` lists the server scripts, which run in that order when the resource starts. Paths are relative to
the folder and must stay inside it.

The API so far is one function:

```lua
sawgrass.on("playerJoin", function(player: number) end)   -- a player entered the world
sawgrass.on("playerLeave", function(player: number) end)  -- a player left; the id may be reused later
sawgrass.on("tick", function(tick: number, dt: number) end)  -- once per server tick; dt in seconds
```

`player` is the player's entity id, the same id clients see in snapshots. Handlers run in the order they were
registered. `print` writes to the server's report, every five seconds.

## Limits

Every resource runs alone in Luau's sandbox: no files, network, processes, `require` or `loadstring`, and the
built-in libraries are read-only. Calls into it are limited (`crates/sawgrass-script`):

- 4 ms per event, then the call is stopped with an error and the next event runs normally;
- 16 MiB of memory;
- 8 ms for all resources together in one tick (`--script-budget-ms`); a resource that gets no time keeps its player
  events for the next tick;
- after 5 overruns in a row (`--script-strikes`, 0 never) the resource is turned off and the server prints why;
  `enable NAME` on the server's standard input turns it back on.

Run them with `sawgrass-server --resources resources`.
