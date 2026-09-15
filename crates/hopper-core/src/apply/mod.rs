//! Carrying out a plan against the server directory, crash-safely.

pub mod execute;
pub mod journal;

pub use execute::{Applied, ApplyError, Summary, apply, next_lockfile, read_lockfile};
pub use journal::{Intent, Journal, JournalError, Recovery, Removal, recover};
