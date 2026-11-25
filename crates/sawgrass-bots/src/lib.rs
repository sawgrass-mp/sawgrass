//! Headless load-test bots for Sawgrass.
//!
//! Each bot connects to a gateway over QUIC exactly as a game client would (same handshake, same packets),
//! moves between waypoints with [`walker::Walker`], sends its state every client tick and reads the snapshots
//! the server sends back. [`stats::Counters`] collects what all bots of a run sent and received, and how late
//! their sends ran, so [`report`] can tell a slow server from a slow load generator. The `sawgrass-bots` binary
//! runs N of them; see the crate README. [`misbehave`] adds bots that flood, teleport, send malformed events or
//! reconnect in a storm, to test the server's defences under load.

pub mod bot;
pub mod cpu;
pub mod misbehave;
pub mod record;
pub mod replay;
pub mod report;
pub mod stats;
pub mod walker;

pub use bot::{BotConfig, Joined, run};
