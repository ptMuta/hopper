//! Turning something the user typed into a concrete set of files to install.

pub mod collection;
pub mod curseforge;
pub mod mrpack;
pub mod resolve;
/// Experimental, off by default: Modrinth publishes no API for shared instances.
#[cfg(feature = "shared-instances")]
pub mod shared;
pub mod spec;

pub use mrpack::stage_overrides;
pub use mrpack::{EnvSupport, FileEnv, Mrpack, MrpackError, MrpackIndex, OverrideKind};
pub use spec::{SourceSpec, SpecError};
