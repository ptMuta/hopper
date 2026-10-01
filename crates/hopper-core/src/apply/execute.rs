//! Carrying out a reconciled plan against the server directory.
//!
//! The ordering here is the crash-safety contract, and every step exists for a reason:
//!
//! ```text
//! 1. write journal.json + fsync     the transaction marker
//! 2. back up anything displaced
//! 3. write, then delete
//! 4. prune emptied directories
//! 5. commit lock.json               atomically
//! 6. remove journal.json
//! ```
//!
//! The only route from "the directory has been modified" to "there is no journal" runs through
//! step 5, so the lockfile can never durably disagree with the disk. Everything needed is
//! already in the content store before step 1 begins, so nothing here can fail for want of a
//! download.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cache::{BlobError, BlobStore};
use crate::fs::{self as hfs, IoPath};
use crate::model::{Digest, LockedFile, Lockfile, RelPath};
use crate::plan::{Decision, reconcile::ConflictResolution};

use super::journal::{Intent, Journal, Removal};

/// Where hopper keeps its own state inside a server directory.
pub const STATE_DIR: &str = ".hopper";

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Io(#[from] IoPath),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("could not write the lockfile: {0}")]
    Lock(String),
    #[error("could not write the journal: {0}")]
    Journal(String),
}

/// What a plan will do, before it does it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub added: usize,
    pub restored: usize,
    pub upgraded: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub adopted: usize,
    pub preserved: usize,
    pub conflicts: usize,
    pub rejected: usize,
    /// Files present that hopper does not manage. Reported every run, because it is the
    /// promise the tool exists to keep.
    pub untouched_user_files: usize,
}

impl Summary {
    pub fn of(decisions: &[(RelPath, Decision)]) -> Self {
        let mut s = Self::default();
        for (_, d) in decisions {
            match d {
                Decision::Add { restore, .. } => {
                    if *restore {
                        s.restored += 1
                    } else {
                        s.added += 1
                    }
                }
                Decision::Replace { .. } => s.upgraded += 1,
                Decision::Remove { .. } => s.removed += 1,
                Decision::Keep { .. } => s.unchanged += 1,
                Decision::Adopt { .. } => s.adopted += 1,
                Decision::Preserve { .. } => s.preserved += 1,
                Decision::Conflict { .. } => s.conflicts += 1,
                Decision::Reject { .. } => s.rejected += 1,
                Decision::Untrack { .. } | Decision::LeaveAlone => {}
            }
        }
        s
    }

    /// Whether anything would be written or deleted.
    pub fn changes_anything(&self) -> bool {
        self.added + self.restored + self.upgraded + self.removed > 0
    }

    pub fn total_changes(&self) -> usize {
        self.added + self.restored + self.upgraded + self.removed
    }
}

/// Outcome of a completed apply.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub written: Vec<RelPath>,
    pub removed: Vec<RelPath>,
    /// `(original, where the operator's copy was kept)`.
    pub backed_up: Vec<(RelPath, RelPath)>,
    /// Conflicts resolved by writing the pack's version beside theirs.
    pub new_files: Vec<RelPath>,
    pub pruned_dirs: Vec<RelPath>,
}

pub fn state_dir(root: &Path) -> PathBuf {
    root.join(STATE_DIR)
}

pub fn lock_path(root: &Path) -> PathBuf {
    state_dir(root).join("lock.json")
}

pub fn journal_path(root: &Path) -> PathBuf {
    state_dir(root).join("journal.json")
}

/// Read the committed lockfile, if the directory is managed at all.
pub fn read_lockfile(root: &Path) -> Result<Option<Lockfile>, ApplyError> {
    let Some(bytes) = hfs::read_optional(&lock_path(root))? else {
        return Ok(None);
    };
    Lockfile::load(&bytes)
        .map(Some)
        .map_err(|e| ApplyError::Lock(e.to_string()))
}

/// Apply a set of decisions.
///
/// `next_lock` is built by the caller and written verbatim on success, so recovery never has to
/// re-plan. Content must already be in `store`.
pub fn apply(
    root: &Path,
    decisions: &[(RelPath, Decision)],
    next_lock: &Lockfile,
    store: &BlobStore,
    txn: &str,
    generator: &str,
    now: &str,
) -> Result<Applied, ApplyError> {
    let state = state_dir(root);
    hfs::create_dir_all(&state)?;

    // 1. Declare intent before touching anything. A file written after this point can be
    //    recognised as ours even if we die before committing the lockfile.
    let mut journal = Journal::new(txn, now, generator, next_lock.clone());
    for (path, decision) in decisions {
        match decision {
            Decision::Add { content, .. } => journal.intended.push(Intent {
                path: path.clone(),
                digest: content.clone(),
                executable: false,
            }),
            Decision::Replace { to, .. } => journal.intended.push(Intent {
                path: path.clone(),
                digest: to.clone(),
                executable: false,
            }),
            Decision::Remove { was } => journal.removing.push(Removal {
                path: path.clone(),
                digest: was.clone(),
            }),
            _ => {}
        }
    }
    let journal_file = journal_path(root);
    hfs::write_atomic(
        &journal_file,
        journal
            .to_json()
            .map_err(|e| ApplyError::Journal(e.to_string()))?
            .as_bytes(),
        false,
    )?;

    let mut out = Applied::default();
    let backup_root = state.join("backups").join(txn);

    // 2 and 3. Writes before deletes, so a path that is being replaced is never briefly absent.
    for (path, decision) in decisions {
        let target = path.resolve_under(root);
        match decision {
            Decision::Add { content, .. } => {
                store.materialize(content, &target, false)?;
                out.written.push(path.clone());
            }
            Decision::Replace { to, .. } => {
                store.materialize(to, &target, false)?;
                out.written.push(path.clone());
            }
            Decision::Conflict {
                resolution,
                desired,
                ..
            } => {
                apply_conflict(
                    root,
                    path,
                    *resolution,
                    desired.as_ref(),
                    store,
                    &backup_root,
                    &mut out,
                )?;
            }
            _ => {}
        }
    }

    for (path, decision) in decisions {
        if let Decision::Remove { was } = decision {
            let target = path.resolve_under(root);
            // Re-check before deleting: if the operator changed the file since we planned, the
            // hash will differ and their edit wins over our tidiness.
            if let Ok((_, actual)) = hfs::hash_file(&target)
                && actual == *was
            {
                hfs::remove_file_if_present(&target)?;
                out.removed.push(path.clone());
            }
        }
    }

    // 4. Tidy directories our removals emptied, deepest first.
    let mut candidates: Vec<RelPath> = out
        .removed
        .iter()
        .flat_map(|p| p.ancestors())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    candidates.sort_by_key(|p| std::cmp::Reverse(p.segments().count()));
    for dir in candidates {
        if crate::plan::is_protected(&dir) {
            continue;
        }
        if hfs::remove_dir_if_empty(&dir.resolve_under(root))? {
            out.pruned_dirs.push(dir);
        }
    }

    // 5. Commit. From here on the lockfile describes the disk.
    hfs::write_atomic(
        &lock_path(root),
        next_lock
            .to_json()
            .map_err(|e| ApplyError::Lock(e.to_string()))?
            .as_bytes(),
        false,
    )?;

    // 6. Transaction over.
    hfs::remove_file_if_present(&journal_file)?;
    hfs::fsync_dir(&state)?;

    Ok(out)
}

fn apply_conflict(
    root: &Path,
    path: &RelPath,
    resolution: ConflictResolution,
    desired: Option<&Digest>,
    store: &BlobStore,
    backup_root: &Path,
    out: &mut Applied,
) -> Result<(), ApplyError> {
    // Nothing to write means nothing to do, whatever the resolution says.
    let Some(wanted) = desired.cloned() else {
        return Ok(());
    };
    let target = path.resolve_under(root);

    match resolution {
        // Never loses anything: their file stays, ours lands beside it for comparison.
        ConflictResolution::KeepOursWriteNew => {
            let Ok(new_path) = path.with_suffix(".new") else {
                return Ok(());
            };
            store.materialize(&wanted, &new_path.resolve_under(root), false)?;
            out.new_files.push(new_path);
        }
        ConflictResolution::BackupThenWrite => {
            let backup = backup_root.join(path.as_str());
            if let Some(parent) = backup.parent() {
                hfs::create_dir_all(parent)?;
            }
            if let Some(bytes) = hfs::read_optional(&target)? {
                hfs::write_atomic(&backup, &bytes, false)?;
                out.backed_up
                    .push((path.clone(), RelPath::parse(path.as_str()).unwrap()));
            }
            store.materialize(&wanted, &target, false)?;
            out.written.push(path.clone());
        }
        ConflictResolution::Overwrite => {
            store.materialize(&wanted, &target, false)?;
            out.written.push(path.clone());
        }
        ConflictResolution::Skip => {}
    }
    Ok(())
}

/// Build the lockfile that should exist once `decisions` have been applied.
pub fn next_lockfile(
    previous: Option<&Lockfile>,
    decisions: &[(RelPath, Decision)],
    entries: &BTreeMap<RelPath, LockedFile>,
    template: Lockfile,
) -> Lockfile {
    let mut files: Vec<LockedFile> = Vec::new();

    for (path, decision) in decisions {
        let existing = previous.and_then(|l| l.file(path));
        match decision {
            Decision::Add { .. } | Decision::Replace { .. } | Decision::Adopt { .. } => {
                if let Some(e) = entries.get(path) {
                    files.push(e.clone());
                }
            }
            Decision::Keep { .. } => {
                if let Some(e) = existing {
                    files.push(e.clone());
                }
            }
            Decision::Preserve { on_disk, .. } => {
                // Record the edit so we report it once and stay quiet afterwards.
                if let Some(e) = existing {
                    let mut e = e.clone();
                    e.user_modified = Some(on_disk.clone());
                    files.push(e);
                }
            }
            Decision::Conflict { resolution, .. } => match resolution {
                ConflictResolution::BackupThenWrite | ConflictResolution::Overwrite => {
                    if let Some(e) = entries.get(path) {
                        files.push(e.clone());
                    }
                }
                _ => {
                    if let Some(e) = existing {
                        files.push(e.clone());
                    }
                }
            },
            // Removed, untracked, rejected, or never ours: not in the new lockfile.
            Decision::Remove { .. }
            | Decision::Untrack { .. }
            | Decision::LeaveAlone
            | Decision::Reject { .. } => {}
        }
    }

    Lockfile {
        files: Lockfile::sorted(files),
        ..template
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileState, HashAlgo, Managed, OverrideLayer, Provenance};

    fn d(seed: u8) -> Digest {
        Digest::new(HashAlgo::Sha512, &format!("{seed:0128x}")).unwrap()
    }

    fn p(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    fn summary_of(decisions: &[(RelPath, Decision)]) -> Summary {
        Summary::of(decisions)
    }

    #[test]
    fn summary_counts_each_kind_of_change() {
        let decisions = vec![
            (
                p("mods/a.jar"),
                Decision::Add {
                    content: d(1),
                    restore: false,
                },
            ),
            (
                p("mods/b.jar"),
                Decision::Add {
                    content: d(2),
                    restore: true,
                },
            ),
            (
                p("mods/c.jar"),
                Decision::Replace {
                    from: d(3),
                    to: d(4),
                },
            ),
            (p("mods/d.jar"), Decision::Remove { was: d(5) }),
            (
                p("mods/e.jar"),
                Decision::Keep {
                    digest: d(6),
                    disabled: false,
                },
            ),
            (p("mods/f.jar"), Decision::LeaveAlone),
        ];
        let s = summary_of(&decisions);
        assert_eq!(s.added, 1);
        assert_eq!(s.restored, 1);
        assert_eq!(s.upgraded, 1);
        assert_eq!(s.removed, 1);
        assert_eq!(s.unchanged, 1);
        assert_eq!(s.total_changes(), 4);
        assert!(s.changes_anything());
    }

    #[test]
    fn a_settled_directory_reports_no_changes() {
        let decisions = vec![(
            p("mods/a.jar"),
            Decision::Keep {
                digest: d(1),
                disabled: false,
            },
        )];
        assert!(!summary_of(&decisions).changes_anything());
    }

    #[test]
    fn preserved_and_adopted_files_are_not_counted_as_changes() {
        // Neither writes anything, so neither should make an update look like it did work.
        let decisions = vec![
            (
                p("config/a.toml"),
                Decision::Preserve {
                    lock: d(1),
                    on_disk: d(2),
                    first_seen: true,
                },
            ),
            (
                p("mods/b.jar"),
                Decision::Adopt {
                    content: d(3),
                    reconciled: false,
                },
            ),
        ];
        let s = summary_of(&decisions);
        assert!(!s.changes_anything());
        assert_eq!(s.preserved, 1);
        assert_eq!(s.adopted, 1);
    }

    #[test]
    fn state_paths_live_under_a_single_hidden_directory() {
        let root = Path::new("/srv/mc");
        assert_eq!(lock_path(root), Path::new("/srv/mc/.hopper/lock.json"));
        assert_eq!(
            journal_path(root),
            Path::new("/srv/mc/.hopper/journal.json")
        );
    }

    #[test]
    fn an_unmanaged_directory_has_no_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_lockfile(dir.path()).unwrap().is_none());
    }

    fn template() -> Lockfile {
        use crate::model::lock::{PackRecord, PolicyRecord, ServerRecord};
        use crate::model::{LoaderKind, MinecraftVersion};
        Lockfile {
            lock_version: crate::model::LOCK_VERSION,
            generator: "hopper-test".into(),
            updated_at: "2026-09-15T00:00:00Z".into(),
            server: ServerRecord {
                minecraft: MinecraftVersion::new("26.3"),
                loader: LoaderKind::Fabric,
                loader_version: "0.17.2".into(),
                java_major: 25,
                start_script: None,
            },
            pack: PackRecord {
                name: "T".into(),
                version_label: None,
                source_arg: "t".into(),
                ..Default::default()
            },
            policy: PolicyRecord::default(),
            files: vec![],
            skipped: vec![],
            unknown: Default::default(),
        }
    }

    fn entry(path: &str, digest: Digest) -> LockedFile {
        LockedFile {
            path: p(path),
            digest,
            size: 1,
            state: FileState::Managed,
            provenance: Provenance::Override {
                layer: OverrideLayer::Global,
            },
            managed: Managed::Full,
            executable: false,
            user_modified: None,
            disabled: false,
            mtime_ns: None,
        }
    }

    #[test]
    fn the_next_lockfile_drops_removed_files_and_keeps_the_rest() {
        let mut prev = template();
        prev.files = Lockfile::sorted(vec![
            entry("mods/keep.jar", d(1)),
            entry("mods/drop.jar", d(2)),
        ]);

        let decisions = vec![
            (
                p("mods/keep.jar"),
                Decision::Keep {
                    digest: d(1),
                    disabled: false,
                },
            ),
            (p("mods/drop.jar"), Decision::Remove { was: d(2) }),
            (
                p("mods/new.jar"),
                Decision::Add {
                    content: d(3),
                    restore: false,
                },
            ),
        ];
        let entries = [(p("mods/new.jar"), entry("mods/new.jar", d(3)))]
            .into_iter()
            .collect();

        let next = next_lockfile(Some(&prev), &decisions, &entries, template());
        let paths: Vec<&str> = next.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["mods/keep.jar", "mods/new.jar"]);
    }

    #[test]
    fn a_preserved_edit_is_recorded_so_it_is_not_reported_twice() {
        let mut prev = template();
        prev.files = vec![entry("config/a.toml", d(1))];
        let decisions = vec![(
            p("config/a.toml"),
            Decision::Preserve {
                lock: d(1),
                on_disk: d(9),
                first_seen: true,
            },
        )];
        let next = next_lockfile(Some(&prev), &decisions, &BTreeMap::new(), template());
        assert_eq!(next.files[0].user_modified, Some(d(9)));
    }

    #[test]
    fn a_skipped_conflict_leaves_the_previous_entry_intact() {
        let mut prev = template();
        prev.files = vec![entry("config/a.toml", d(1))];
        let decisions = vec![(
            p("config/a.toml"),
            Decision::Conflict {
                reason: crate::plan::ConflictReason::LocalModification,
                resolution: ConflictResolution::KeepOursWriteNew,
                desired: Some(d(2)),
                on_disk: Some(d(9)),
            },
        )];
        let next = next_lockfile(Some(&prev), &decisions, &BTreeMap::new(), template());
        assert_eq!(next.files.len(), 1);
        assert_eq!(
            next.files[0].digest,
            d(1),
            "we did not write, so nothing changed"
        );
    }

    #[test]
    fn the_lockfile_is_sorted_and_free_of_duplicates() {
        let decisions = vec![
            (
                p("mods/z.jar"),
                Decision::Add {
                    content: d(1),
                    restore: false,
                },
            ),
            (
                p("config/a.toml"),
                Decision::Add {
                    content: d(2),
                    restore: false,
                },
            ),
        ];
        let entries = [
            (p("mods/z.jar"), entry("mods/z.jar", d(1))),
            (p("config/a.toml"), entry("config/a.toml", d(2))),
        ]
        .into_iter()
        .collect();
        let next = next_lockfile(None, &decisions, &entries, template());
        assert!(next.validate().is_ok());
        assert_eq!(next.files[0].path.as_str(), "config/a.toml");
    }
}
