//! The Sawgrass world server.
//!
//! The world lives in an ECS ([`hecs::World`]) and advances in fixed-rate ticks driven by [`TickLoop`]
//! (task SRV-1). Every tick's duration is recorded in [`TickMetrics`], which reports p50, p99 and the
//! maximum over a rolling window against the budget in `docs/architecture.md` (p99 under 70% of the tick
//! interval).

pub mod metrics;
pub mod tick;

pub use hecs;
pub use metrics::{TickMetrics, TickStats};
pub use tick::{TickConfig, TickConfigError, TickInfo, TickLoop};
