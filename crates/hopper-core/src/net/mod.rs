//! Network-facing concerns: what we are willing to talk to, and how.

pub mod allowlist;
pub mod http;

pub use allowlist::{HostAllowlist, HostError, PACK_HOSTS, RUNTIME_HOSTS};
pub use http::{HttpClient, HttpError};
