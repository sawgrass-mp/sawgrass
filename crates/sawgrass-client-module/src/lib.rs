//! The game-side Sawgrass module (stage 3 of docs/roadmap.md, step 7).
//!
//! It runs inside a runtime that already runs the game, VIctory first and XCL later (ADR 0004), and is loaded only by
//! that runtime's own API. It never loads or injects itself, and nothing in it works around protection or keys.
//!
//! - [`runtime`]: the boundary each runtime adapter implements, and the build identity of the running game.
//! - [`victory`]: the VIctory adapter, a stub until VIctory has a module-loading and hook API.
//! - [`pattern`] and [`table`]: functions are located at run time by pattern, from a table written for one game
//!   build and kept outside this repository.
//! - [`fake`]: a runtime over a made-up code image, so all of the above is tested without the game.

pub mod fake;
pub mod pattern;
pub mod runtime;
pub mod table;
pub mod victory;
