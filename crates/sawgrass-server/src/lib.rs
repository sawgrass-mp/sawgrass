//! The Sawgrass world server.
//!
//! The world lives in an ECS ([`hecs::World`]) and advances in fixed-rate ticks driven by [`TickLoop`]
//! (task SRV-1). Every tick's duration is recorded in [`TickMetrics`], which reports p50, p99 and the
//! maximum over a rolling window against the budget in `docs/architecture.md` (p99 under 70% of the tick
//! interval). [`interest`] decides which entities each player sees (task IM-1). [`net`] connects players
//! through an in-process gateway and sends them snapshots. [`slots`] gives every entity a dense, recycled index.
//! [`scripts`] runs the Luau resources in the tick.

pub mod combat;
pub mod cpu;
pub mod interest;
pub mod limits;
pub mod metrics;
pub mod money;
pub mod movement;
pub mod net;
pub mod owned;
pub mod ownership;
pub mod recording;
pub mod scripts;
pub mod seats;
pub mod slots;
pub mod snapshot;
pub mod tick;

pub use hecs;
pub use metrics::{TickMetrics, TickStats};
pub use tick::{
    BETWEEN, Clock, Overrun, SystemClock, SystemPanic, TickConfig, TickConfigError, TickInfo, TickLoop,
};
