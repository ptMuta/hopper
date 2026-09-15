//! Talking to registries and metadata services.

pub mod client;
pub mod modrinth;
pub mod mojang;
pub mod ratelimit;

pub use ratelimit::{Budget, Clock, RateGate, RetryPolicy, SystemClock};
