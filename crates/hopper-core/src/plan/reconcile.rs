//! The three-way reconcile: lockfile ⟂ disk ⟂ desired.
//!
//! This is the product's reason to exist. Every other tool in the ecosystem either overwrites
//! blindly (clobbering operator-edited configs) or never deletes (leaving orphaned jars that
//! crash the server after a pack drops a mod). Doing both correctly means asking three
//! questions about every path, not two.
//!
//! The function is deliberately **pure**: no I/O, no clock, no network. That is what lets
//! `--dry-run`, the confirmation diff, `status` and `--json` all render the identical value
//! that apply will consume — there is no second code path to drift out of sync.
//!
//! # Two guarantees
//!
//! 1. **A file hopper did not install is never deleted.** Enforced structurally: deletion is
//!    only reachable from a branch where the lockfile has a `Managed` entry whose digest
//!    matches the bytes on disk.
//! 2. **A file the operator edited is never silently overwritten.** Enforced by comparing the
//!    disk digest against the lockfile's, and routing every mismatch through an explicit
//!    conflict decision.

use crate::model::{Digest, FileState, LockedFile, Managed, Provenance, RelPath};

use super::state::{DesiredFile, DiskEntry, DiskState, EntryKind};

/// What to do with one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The operator's own file. Not tracked, not touched, not reported as a problem.
    LeaveAlone,
    /// Write it. `restore` distinguishes "new in this pack" from "we had it, it went missing".
    Add { content: Digest, restore: bool },
    /// Content is already correct — record ownership without writing a byte. `reconciled` means
    /// the operator had hand-applied this exact update before we got here.
    Adopt { content: Digest, reconciled: bool },
    /// Straightforward upgrade.
    Replace { from: Digest, to: Digest },
    /// Delete. Only ever reached for a `Managed` file whose disk bytes are ours.
    Remove { was: Digest },
    /// Tracked and unchanged. Counted in the summary, never printed per-file.
    Keep { digest: Digest, disabled: bool },
    /// Operator edited it and the pack did not change it: keep theirs, say so once.
    Preserve {
        lock: Digest,
        on_disk: Digest,
        first_seen: bool,
    },
    /// Stop tracking, leave the bytes alone.
    Untrack { reason: UntrackReason },
    /// Needs a decision; `resolution` says what we will do absent further instruction.
    ///
    /// Carries both sides of the disagreement so the plan is self-contained: rendering the
    /// diff and applying it both work from this value alone, with nothing re-derived.
    Conflict {
        reason: ConflictReason,
        resolution: ConflictResolution,
        /// What the pack wants here, when it wants anything.
        desired: Option<Digest>,
        /// What is on disk now, when we were able to read it.
        on_disk: Option<Digest>,
    },
    /// Refused outright. Never resolved automatically.
    Reject { reason: RejectReason },
}

impl Decision {
    /// Whether applying this writes to or deletes from the server directory.
    pub fn mutates(&self) -> bool {
        match self {
            Self::Add { .. } | Self::Replace { .. } | Self::Remove { .. } => true,
            Self::Conflict { resolution, .. } => resolution.writes(),
            Self::LeaveAlone
            | Self::Adopt { .. }
            | Self::Keep { .. }
            | Self::Preserve { .. }
            | Self::Untrack { .. }
            | Self::Reject { .. } => false,
        }
    }

    /// Whether the operator should be told about this, as opposed to it being folded into a count.
    pub fn is_noteworthy(&self) -> bool {
        match self {
            Self::LeaveAlone | Self::Keep { .. } => false,
            Self::Preserve { first_seen, .. } => *first_seen,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackReason {
    /// Already gone from disk and no longer wanted.
    AlreadyGone,
    /// The pack dropped it but the operator had edited it — leave their work alone.
    RemovedButModified,
    /// It was the operator's file that the pack merely happened to want. Never ours to delete.
    WasAdopted,
    /// The operator deleted it and asked us not to put it back.
    UserDeleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictReason {
    /// Lockfile, disk and pack all disagree: edited locally *and* changed upstream.
    LocalModification,
    /// An untracked file already occupies a path the pack wants, with different content.
    UntrackedCollision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// A directory sits where the pack wants a file. We will not remove a tree to make room.
    DirectoryInTheWay,
    /// A symlink at the target: writing through it would escape the managed directory.
    SymlinkInTheWay,
    /// Not a regular file (socket, device, fifo).
    NotARegularFile,
    /// Protected content — `world/`, `logs/`, ban lists — which we never write or delete.
    ProtectedPath,
}

/// What a conflict does by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Keep the operator's bytes, write ours alongside as `<path>.new`. Never loses data.
    KeepOursWriteNew,
    /// Move theirs into `.hopper/backups/<txn>/` and write the pack's version.
    BackupThenWrite,
    /// Overwrite. Only for artifacts nobody hand-edits (loader libraries, the server jar).
    Overwrite,
    /// Leave everything as it is.
    Skip,
}

impl ConflictResolution {
    pub fn writes(self) -> bool {
        matches!(
            self,
            Self::BackupThenWrite | Self::Overwrite | Self::KeepOursWriteNew
        )
    }
}

/// How conflicts resolve, keyed on **provenance** rather than on path shape.
///
/// Guessing from the path (`if path.starts_with("config/")`) breaks the moment a pack puts
/// config somewhere unusual. Where a file came from is the durable signal: a mod jar is
/// replaceable, an operator's tuning file is not.
#[derive(Debug, Clone)]
pub struct ConflictPolicy {
    /// Overrides the provenance-derived default when set.
    pub force: Option<ConflictResolution>,
    /// Put back files the pack wants that have gone missing from disk.
    pub restore_deleted: bool,
    /// Take ownership of an untracked file whose content differs, instead of refusing.
    pub adopt_collisions: bool,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self {
            force: None,
            restore_deleted: true,
            adopt_collisions: false,
        }
    }
}

impl ConflictPolicy {
    pub fn resolution_for(&self, provenance: &Provenance, managed: Managed) -> ConflictResolution {
        if let Some(force) = self.force {
            return force;
        }
        match (provenance, managed) {
            // Written once, then the operator's business forever.
            (_, Managed::ContentSeed) => ConflictResolution::Skip,
            // Their tuning beats the pack's defaults; ours lands as `.new` so it is not lost.
            (Provenance::Override { .. }, _) => ConflictResolution::KeepOursWriteNew,
            (Provenance::Generated { .. }, _) => ConflictResolution::KeepOursWriteNew,
            // The pack's jar has to win or the server is running something the pack never
            // tested — but keep a copy, because someone patched that jar on purpose.
            (Provenance::Registry { .. } | Provenance::PackFile { .. }, _) => {
                ConflictResolution::BackupThenWrite
            }
            // Machine-generated, not hand-edited.
            (Provenance::Loader { .. } | Provenance::ServerJar { .. }, _) => {
                ConflictResolution::Overwrite
            }
        }
    }
}

/// Everything known about one path, assembled by [`reconcile`].
#[derive(Debug, Clone, Copy)]
pub struct Triple<'a> {
    pub path: &'a RelPath,
    pub lock: Option<&'a LockedFile>,
    pub disk: Option<&'a DiskEntry>,
    pub want: Option<&'a DesiredFile>,
    pub hints: Hints,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Hints {
    /// `<path>.disabled` exists — the operator switched this mod off deliberately.
    pub disabled_marker: bool,
    /// The path is protected: worlds, logs, ban lists. Never written, never deleted.
    pub protected: bool,
}

/// Decide what to do with one path.
///
/// The presence of (lockfile, disk, desired) gives eight combinations. The all-present case
/// splits further by digest equality — and since equality is transitive, only five of its eight
/// boolean triples are reachable, which the `unreachable!` arms both document and enforce.
pub fn classify(t: &Triple<'_>, pol: &ConflictPolicy) -> Decision {
    // Guards first: these are refusals, not decisions, and must not be reachable from any
    // branch below.
    if let Some(d) = t.disk {
        match d.kind {
            EntryKind::Dir if t.want.is_some() => {
                return Decision::Reject {
                    reason: RejectReason::DirectoryInTheWay,
                };
            }
            // Never followed, never replaced — even when we are the ones tracking it.
            EntryKind::Symlink => {
                return Decision::Reject {
                    reason: RejectReason::SymlinkInTheWay,
                };
            }
            EntryKind::Other => {
                return Decision::Reject {
                    reason: RejectReason::NotARegularFile,
                };
            }
            _ => {}
        }
    }
    // Protected paths are inert unless we are merely adding something that is not there.
    if t.hints.protected && (t.disk.is_some() || t.want.is_none()) {
        return Decision::Reject {
            reason: RejectReason::ProtectedPath,
        };
    }

    match (t.lock, t.disk, t.want) {
        // Nothing anywhere.
        (None, None, None) => Decision::LeaveAlone,

        // Fresh install of a file we have never seen.
        (None, None, Some(w)) => Decision::Add {
            content: w.content.clone(),
            restore: false,
        },

        // THE GUARANTEE: an untracked file the pack does not want is the operator's. We do not
        // record it, report it as a problem, or ever remove it.
        (None, Some(_), None) => Decision::LeaveAlone,

        // An untracked file sits where the pack wants one.
        (None, Some(d), Some(w)) => match d.digest.as_ref() {
            // Byte-identical already: take ownership, write nothing. Marked `Adopted`, so if
            // the pack later drops it we untrack rather than delete — it was theirs first.
            Some(dd) if *dd == w.content => Decision::Adopt {
                content: w.content.clone(),
                reconciled: false,
            },
            Some(dd) if pol.adopt_collisions => Decision::Conflict {
                reason: ConflictReason::UntrackedCollision,
                resolution: ConflictResolution::BackupThenWrite,
                desired: Some(w.content.clone()),
                on_disk: Some(dd.clone()),
            },
            Some(dd) => Decision::Conflict {
                reason: ConflictReason::UntrackedCollision,
                resolution: ConflictResolution::Skip,
                desired: Some(w.content.clone()),
                on_disk: Some(dd.clone()),
            },
            // Existence known but content not read; treat as a collision rather than assume.
            None => Decision::Conflict {
                reason: ConflictReason::UntrackedCollision,
                resolution: ConflictResolution::Skip,
                desired: Some(w.content.clone()),
                on_disk: None,
            },
        },

        // Tracked, gone from disk, no longer wanted.
        (Some(_), None, None) => Decision::Untrack {
            reason: UntrackReason::AlreadyGone,
        },

        // Tracked, gone from disk, still wanted.
        (Some(l), None, Some(w)) => {
            if t.hints.disabled_marker {
                // Renaming to `.disabled` is how you switch a mod off. Restoring it would
                // silently undo that on every update, so the marker wins permanently.
                Decision::Keep {
                    digest: l.digest.clone(),
                    disabled: true,
                }
            } else if pol.restore_deleted {
                Decision::Add {
                    content: w.content.clone(),
                    restore: true,
                }
            } else {
                Decision::Untrack {
                    reason: UntrackReason::UserDeleted,
                }
            }
        }

        // Tracked and present, but the pack no longer wants it.
        (Some(l), Some(d), None) => {
            // Never ours to delete: adopted files and seed files stay.
            if l.state == FileState::Adopted {
                return Decision::Untrack {
                    reason: UntrackReason::WasAdopted,
                };
            }
            if l.managed == Managed::ContentSeed {
                return Decision::Keep {
                    digest: l.digest.clone(),
                    disabled: false,
                };
            }
            match d.digest.as_ref() {
                // Our bytes, unwanted: this is the orphan cleanup nothing else does.
                Some(dd) if l.is_ours(dd) => Decision::Remove {
                    was: l.digest.clone(),
                },
                // They changed it and the pack dropped it — their edit outranks our tidiness.
                Some(_) => Decision::Untrack {
                    reason: UntrackReason::RemovedButModified,
                },
                None => Decision::Untrack {
                    reason: UntrackReason::RemovedButModified,
                },
            }
        }

        // Tracked, present, and still wanted — the interesting case.
        (Some(l), Some(d), Some(w)) => {
            let Some(dd) = d.digest.as_ref() else {
                // In scope but unhashed should not happen; refuse rather than guess.
                return Decision::Conflict {
                    reason: ConflictReason::LocalModification,
                    resolution: ConflictResolution::Skip,
                    desired: Some(w.content.clone()),
                    on_disk: None,
                };
            };

            // An edit we already acknowledged: preserve silently so we do not nag every run.
            if l.is_known_edit(dd) {
                return Decision::Preserve {
                    lock: l.digest.clone(),
                    on_disk: dd.clone(),
                    first_seen: false,
                };
            }

            // The operator's own file that the pack also ships: never overwrite it.
            if l.state == FileState::Adopted {
                return if *dd == w.content {
                    Decision::Keep {
                        digest: w.content.clone(),
                        disabled: false,
                    }
                } else {
                    Decision::Conflict {
                        reason: ConflictReason::UntrackedCollision,
                        resolution: ConflictResolution::Skip,
                        desired: Some(w.content.clone()),
                        on_disk: Some(dd.clone()),
                    }
                };
            }

            if l.managed == Managed::ContentSeed {
                // Seeded once; content is theirs from then on.
                return Decision::Keep {
                    digest: l.digest.clone(),
                    disabled: false,
                };
            }

            match (l.is_ours(dd), *dd == w.content, l.digest == w.content) {
                // Untouched and current.
                (true, true, true) => Decision::Keep {
                    digest: l.digest.clone(),
                    disabled: false,
                },
                // Untouched, pack moved on: a clean upgrade.
                (true, false, false) => Decision::Replace {
                    from: l.digest.clone(),
                    to: w.content.clone(),
                },
                // Disk already matches the new version — they applied it by hand. Fix the
                // bookkeeping, write nothing.
                (false, true, false) => Decision::Adopt {
                    content: w.content.clone(),
                    reconciled: true,
                },
                // Edited locally, pack unchanged: preserve, and say so this once.
                (false, false, true) => Decision::Preserve {
                    lock: l.digest.clone(),
                    on_disk: dd.clone(),
                    first_seen: true,
                },
                // Edited locally *and* changed upstream: the genuine conflict.
                (false, false, false) => Decision::Conflict {
                    reason: ConflictReason::LocalModification,
                    resolution: pol.resolution_for(&w.provenance, w.managed),
                    desired: Some(w.content.clone()),
                    on_disk: Some(dd.clone()),
                },
                // Equality is transitive: any two of these imply the third, so the remaining
                // three combinations cannot occur. Compiles to nothing; documents the proof.
                (true, true, false) | (true, false, true) | (false, true, true) => {
                    unreachable!("digest equality is transitive")
                }
            }
        }
    }
}

/// Paths hopper never writes to or deletes, whatever the pack says.
///
/// Losing a world to a modpack update is unrecoverable, so this is a hard structural guard
/// rather than a policy knob.
pub const PROTECTED_DIRS: &[&str] = &["world", "logs", "crash-reports", "backups", ".hopper"];
pub const PROTECTED_FILES: &[&str] = &[
    "ops.json",
    "whitelist.json",
    "banned-players.json",
    "banned-ips.json",
    "usercache.json",
];

pub fn is_protected(path: &RelPath) -> bool {
    // `world`, `world_nether`, `world_the_end`, and anything an operator named `world-backup`.
    let first = path.segments().next().unwrap_or("");
    if first.starts_with("world") {
        return true;
    }
    if PROTECTED_DIRS.iter().any(|d| path.starts_with_dir(d)) {
        return true;
    }
    PROTECTED_FILES.contains(&path.as_str())
}

/// Assemble the union of tracked, present and wanted paths, and classify each one.
///
/// Returns decisions in path order so the rendered diff and the resulting lockfile are
/// deterministic.
pub fn reconcile(
    lock: Option<&crate::model::Lockfile>,
    disk: &DiskState,
    desired: &super::state::DesiredSet,
    pol: &ConflictPolicy,
) -> Vec<(RelPath, Decision)> {
    let mut paths: std::collections::BTreeSet<RelPath> = std::collections::BTreeSet::new();
    if let Some(l) = lock {
        paths.extend(l.files.iter().map(|f| f.path.clone()));
    }
    paths.extend(desired.paths().cloned());
    // Only disk paths that are tracked or wanted matter; everything else is the operator's and
    // is intentionally absent from the result rather than classified as `LeaveAlone`.
    paths.retain(|p| lock.is_some_and(|l| l.file(p).is_some()) || desired.get(p).is_some());

    paths
        .into_iter()
        .map(|path| {
            let t = Triple {
                path: &path,
                lock: lock.and_then(|l| l.file(&path)),
                disk: disk.get(&path),
                want: desired.get(&path),
                hints: Hints {
                    disabled_marker: disk.has_disabled_marker(&path),
                    protected: is_protected(&path),
                },
            };
            let d = classify(&t, pol);
            (path, d)
        })
        .collect()
}
