//! Carrying out a plan against the server directory, crash-safely.

pub mod journal;

pub use journal::{Intent, Journal, JournalError, Recovery, Removal, recover};
