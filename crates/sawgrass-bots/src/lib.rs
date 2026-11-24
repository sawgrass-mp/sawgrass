//! Headless load-test bots for Sawgrass.
//!
//! Each bot connects to a gateway over QUIC exactly as a game client would (same handshake, same packets),
//! moves between waypoints with [`walker::Walker`], sends its state every client tick and reads the snapshots
//! the server sends back. [`stats::Counters`] collects what all bots of a run sent and received, and how late
//! their sends ran, so a report can tell a slow server from a slow load generator.

pub mod bot;
pub mod cpu;
pub mod stats;
pub mod walker;

pub use bot::{BotConfig, Joined, run};
