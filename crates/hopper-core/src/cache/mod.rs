//! Shared, content-addressed caching across every server on the machine.

pub mod blobs;

pub use blobs::{Blob, BlobError, BlobStore};
