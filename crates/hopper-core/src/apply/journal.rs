//! Crash safety for the server-directory transaction.
//!
//! # Why there is no replay engine
//!
//! The obvious design is a write-ahead log that gets replayed after a crash. We do something
//! smaller: the journal records only what the run *intended* to place, and recovery is the
//! ordinary reconcile pass with one extra rule.
//!
//! > A file on disk with no lockfile entry whose `(path, digest)` appears in the journal's
//! > `intended` list was written by **us**, not by the operator.
//!
//! That single rule closes the dangerous window. Between "we wrote a file" and "we recorded it
//! in the lockfile", a crash would otherwise leave a file that looks operator-added forever —
//! so we would never manage it and never clean it up. With the rule, a half-applied state is
//! just another disk state, and the next run converges through the matrix that is already
//! tested. No replay logic, no rollback logic, and no way for the lockfile to disagree with
//! the disk.
//!
//! # The ordering that makes it work
//!
//! ```text
//! 1. flock(.hopper/dirlock)              one hopper per directory
//! 2. write journal.json + fsync          the transaction marker
//! 3. mutate the server directory         backups, writes, deletes
//! 4. fsync mutated directories
//! 5. lock.json.tmp -> fsync -> rename    commit
//! 6. unlink journal.json + fsync         transaction over
//! ```
//!
//! The only path from "disk mutated" to "no journal" runs through step 5, so the lockfile can
//! never durably disagree with the disk.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{Digest, LockedFile, RelPath};

pub const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error(
        "a previous run was interrupted by a newer hopper (journal v{found}, this build reads v{supported}); upgrade hopper and re-run"
    )]
    TooNew { found: u32, supported: u32 },
    #[error("journal is malformed: {0}")]
    Malformed(String),
}

/// A file the run intends to place, and the exact bytes it intends to place there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub path: RelPath,
    /// sha512 of the content being written. Recovery matches on this, so a file whose content
    /// differs is *not* claimed as ours.
    pub digest: Digest,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

/// A file the run intends to delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    pub path: RelPath,
    /// The bytes we expect to find. If the operator changed the file after we planned, the
    /// digest will not match and the deletion is abandoned rather than forced.
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub journal_version: u32,
    pub txn: String,
    pub started_at: String,
    pub generator: String,
    /// Every file this run means to write. The field recovery depends on.
    pub intended: Vec<Intent>,
    pub removing: Vec<Removal>,
    /// `(original, backup)` pairs already displaced into `.hopper/backups/<txn>/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<(RelPath, RelPath)>,
    /// The lockfile to commit once the mutations are done, so recovery never has to re-plan.
    pub next_lock: crate::model::Lockfile,
}

impl Journal {
    pub fn new(
        txn: impl Into<String>,
        started_at: impl Into<String>,
        generator: impl Into<String>,
        next_lock: crate::model::Lockfile,
    ) -> Self {
        Self {
            journal_version: JOURNAL_VERSION,
            txn: txn.into(),
            started_at: started_at.into(),
            generator: generator.into(),
            intended: Vec::new(),
            removing: Vec::new(),
            backups: Vec::new(),
            next_lock,
        }
    }

    pub fn load(bytes: &[u8]) -> Result<Self, JournalError> {
        let v: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| JournalError::Malformed(e.to_string()))?;
        let found = v
            .get("journal_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| JournalError::Malformed("missing journal_version".into()))?
            as u32;
        if found > JOURNAL_VERSION {
            return Err(JournalError::TooNew {
                found,
                supported: JOURNAL_VERSION,
            });
        }
        serde_json::from_value(v).map_err(|e| JournalError::Malformed(e.to_string()))
    }

    pub fn to_json(&self) -> Result<String, JournalError> {
        serde_json::to_string_pretty(self).map_err(|e| JournalError::Malformed(e.to_string()))
    }

    /// The recovery rule: did this run write these exact bytes to this path?
    ///
    /// Matching on content as well as path is what keeps the claim honest — if the operator
    /// happened to put a *different* file at a path we intended to write, it stays theirs.
    pub fn claims(&self, path: &RelPath, digest: &Digest) -> bool {
        self.intended
            .iter()
            .any(|i| i.path == *path && i.digest == *digest)
    }

    /// Synthesize lockfile entries for files an interrupted run wrote but never recorded.
    ///
    /// Feeding these into reconcile as if they had been in the lockfile is what lets the
    /// ordinary matrix finish the job: the files become ours, so they can be upgraded or
    /// cleaned up instead of being stranded as untouchable operator files.
    pub fn adopt_orphans<'a>(
        &self,
        committed: Option<&crate::model::Lockfile>,
        on_disk: impl Iterator<Item = (&'a RelPath, &'a Digest)>,
    ) -> Vec<LockedFile> {
        let mut out = Vec::new();
        for (path, digest) in on_disk {
            // Already tracked: nothing to recover.
            if committed.is_some_and(|l| l.file(path).is_some()) {
                continue;
            }
            if !self.claims(path, digest) {
                continue;
            }
            // The intended lockfile already describes this file correctly; reuse its entry
            // rather than inventing provenance.
            if let Some(entry) = self.next_lock.file(path) {
                out.push(entry.clone());
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

/// What a recovery pass concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No journal: the last run finished, or never mutated anything.
    Nothing,
    /// A journal exists and these orphaned files are ours to manage again.
    Resumed { adopted: Vec<LockedFile> },
    /// A journal exists but we cannot read it. The operator has to decide.
    NeedsOperator { reason: String },
}

/// Decide what to do about a journal found at startup.
///
/// Pure: the caller supplies the journal bytes and the observed disk contents, so the whole
/// recovery decision is testable without killing a process.
pub fn recover(
    journal_bytes: Option<&[u8]>,
    committed: Option<&crate::model::Lockfile>,
    disk: &BTreeMap<RelPath, Digest>,
) -> Recovery {
    let Some(bytes) = journal_bytes else {
        return Recovery::Nothing;
    };
    match Journal::load(bytes) {
        Ok(j) => Recovery::Resumed {
            adopted: j.adopt_orphans(committed, disk.iter()),
        },
        // A journal we cannot parse might have been written by a future version, or truncated
        // by the very crash we are recovering from. Either way, guessing is worse than asking.
        Err(e) => Recovery::NeedsOperator {
            reason: e.to_string(),
        },
    }
}
