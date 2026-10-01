//! Installing mod loaders.
//!
//! Loaders differ enormously in how they install — Fabric is a metadata fetch, NeoForge runs a
//! bytecode-patching installer — but they are abstracted by their *result*: a set of files plus
//! a [`LaunchProfile`]. Keeping the abstraction at the output means the difference does not
//! leak into update, cleanup or launch.

pub mod fabric;
pub mod installer;
pub mod launch;
pub mod maven;
pub mod versions;

pub use launch::LaunchProfile;
pub use maven::{Coordinate, MavenError};
pub use versions::{ForgePromotions, neoforge_versions_for};
