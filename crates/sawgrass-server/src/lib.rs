//! The Sawgrass world server.
//!
//! The world lives in an ECS ([`hecs::World`]) and advances in fixed-rate ticks driven by [`TickLoop`]
//! (task SRV-1). Every tick's duration is recorded in [`TickMetrics`], which reports p50, p99 and the
//! maximum over a rolling window against the budget in `docs/architecture.md` (p99 under 70% of the tick
//! interval). [`interest`] decides which entities each player sees (task IM-1). [`net`] connects players
//! through an in-process gateway and sends them snapshots. [`slots`] gives every entity a dense, recycled index.

pub mod cpu;
pub mod interest;
pub mod metrics;
pub mod movement;
pub mod net;
pub mod slots;
pub mod snapshot;
pub mod tick;

pub use hecs;
pub use metrics::{TickMetrics, TickStats};
pub use tick::{Clock, SystemClock, TickConfig, TickConfigError, TickInfo, TickLoop};
