//! hopper's engine: resolve a pack, plan the change, apply it safely.
//!
//! This crate performs no printing and owns no CLI concerns — the binary crate renders
//! everything. See `hopper::model::path::RelPath` for the untrusted-input boundary and
//! (once landed) `plan::reconcile` for the three-way reconcile that is the product's point.

pub mod api;
pub mod apply;
pub mod model;
pub mod net;
pub mod plan;
pub mod source;

pub use model::{
    Digest, HashAlgo, Hashes, LoaderKind, MinecraftVersion, MultiHasher, ProjectId, RegistryId,
    RelPath, VersionId,
};
