//! Turning a resolved pack plus the state on disk into a reviewable change.

pub mod reconcile;
pub mod state;

pub use reconcile::{
    ConflictPolicy, ConflictReason, ConflictResolution, Decision, RejectReason, Triple,
    UntrackReason, classify, is_protected, reconcile,
};
pub use state::{DesiredFile, DesiredSet, DiskEntry, DiskState, EntryKind, ScanScope};
