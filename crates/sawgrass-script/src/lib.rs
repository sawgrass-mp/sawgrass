//! The Luau runtime for resources (task SCRIPT-3, ADR 0005).
//!
//! A resource is a folder with a `resource.toml` manifest and the server scripts it lists. Each resource gets its own
//! Luau instance with native code generation, created and compiled when the resource loads, never in a tick. Every
//! call into a resource runs in Luau's sandbox, under a deadline and a memory cap:
//!
//! - **Deadline:** Luau's interrupt callback, which Luau calls on loop back-edges, function calls and inside its
//!   pattern matcher, reads the clock every [`CLOCK_EVERY`] calls and stops the script once its deadline has passed.
//!   The stop holds until the call returns, so a script cannot catch it with `pcall` and carry on.
//! - **Memory:** mlua's allocator limit. Running into it raises a Luau error; the process never aborts.
//! - **Sandbox:** read-only built-in libraries and globals, no `require`, `loadstring`, `getfenv` or `setfenv`, and
//!   no file, network or process access. `print` goes to a bounded log that the host drains.
//!
//! A failed call returns a [`CallError`] to the host and the instance stays usable for the next call.
//!
//! The scripting API so far is one global table, `sawgrass`, with `sawgrass.on(event, handler)`. The events are
//! `tick(tick, dt)`, `playerJoin(player)` and `playerLeave(player)`, where `player` is the id the server gives the
//! player's entity.

pub mod guards;
mod host;
mod resource;

pub use guards::SORT_UNGUARDED;
pub use host::{DEFAULT_STRIKES, Host, LoadError, TickReport, load_resource};
pub use resource::{CallError, Event, Limits, Resource, Stats};

/// How many interrupt calls pass between two reads of the clock.
pub const CLOCK_EVERY: u32 = 256;
