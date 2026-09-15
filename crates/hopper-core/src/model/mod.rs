//! Domain types shared by every stage of the pipeline.

pub mod hash;
pub mod ids;
pub mod lock;
pub mod path;

pub use hash::{Digest, HashAlgo, HashError, Hashes, MultiHasher};
pub use ids::{LoaderKind, MinecraftVersion, ProjectId, RegistryId, UnknownLoader, VersionId};
pub use lock::{
    Confidence, FileState, LOCK_VERSION, LockError, LockedFile, Lockfile, Managed, OverrideLayer,
    Provenance, SkipReason, SkippedFile,
};
pub use path::{PathError, RelPath};
