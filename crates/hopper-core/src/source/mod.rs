//! Turning something the user typed into a concrete set of files to install.

pub mod mrpack;
pub mod spec;

pub use mrpack::{EnvSupport, FileEnv, Mrpack, MrpackError, MrpackIndex, OverrideKind};
pub use spec::{SourceSpec, SpecError};
