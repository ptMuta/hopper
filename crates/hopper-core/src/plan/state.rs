//! The three inputs to reconcile: what the lockfile says, what is on disk, what the pack wants.
//!
//! All pure data. Producing a [`DiskState`] touches the filesystem, but nothing in this module
//! does, which is what keeps the decision logic testable without I/O.

use std::collections::{BTreeMap, HashMap};

use crate::model::{Digest, Managed, Provenance, RelPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// Never followed and never overwritten: some operators symlink `mods/` at shared storage,
    /// and writing through the link would escape the directory we were asked to manage.
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskEntry {
    pub kind: EntryKind,
    /// `None` when the path is outside the scan's hashing scope — we know it exists but
    /// deliberately did not read it (see [`ScanScope`]).
    pub digest: Option<Digest>,
    pub size: u64,
    pub executable: bool,
    pub mtime_ns: Option<u64>,
}

impl DiskEntry {
    pub fn file(digest: Digest, size: u64) -> Self {
        Self {
            kind: EntryKind::File,
            digest: Some(digest),
            size,
            executable: false,
            mtime_ns: None,
        }
    }

    pub fn is_regular_file(&self) -> bool {
        self.kind == EntryKind::File
    }
}

/// What the scanner looked at.
///
/// Hashing the whole server directory is not an option — `world/` alone is routinely tens of
/// gigabytes. We hash only paths that could possibly matter: those the lockfile tracks and
/// those the new pack wants. Managed directories are still walked shallowly so orphaned files
/// are noticed, but their contents are recorded without digests unless needed.
#[derive(Debug, Clone, Default)]
pub struct ScanScope {
    /// Paths to hash: (lockfile ∪ desired) ∩ disk.
    pub hash: Vec<RelPath>,
    /// Directories walked to find files we did not expect, e.g. `mods/`, `config/`.
    pub managed_dirs: Vec<String>,
    /// Never entered at all: `world*/`, `logs/`, `backups/`, `crash-reports/`, `.hopper/`.
    pub excluded_dirs: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DiskState {
    entries: BTreeMap<RelPath, DiskEntry>,
    /// Lowercased path → real path, for spotting collisions that only bite on
    /// case-insensitive filesystems.
    fold_index: HashMap<String, RelPath>,
}

impl DiskState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: RelPath, entry: DiskEntry) {
        self.fold_index.insert(path.fold_key(), path.clone());
        self.entries.insert(path, entry);
    }

    pub fn get(&self, path: &RelPath) -> Option<&DiskEntry> {
        self.entries.get(path)
    }

    pub fn contains(&self, path: &RelPath) -> bool {
        self.entries.contains_key(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &RelPath> {
        self.entries.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RelPath, &DiskEntry)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A different path that would occupy the same filesystem slot on APFS or NTFS.
    pub fn case_collision(&self, path: &RelPath) -> Option<&RelPath> {
        self.fold_index
            .get(&path.fold_key())
            .filter(|existing| *existing != path)
    }

    /// True when `<path>.disabled` exists, meaning the operator switched this mod off by hand
    /// and we must not restore it.
    pub fn has_disabled_marker(&self, path: &RelPath) -> bool {
        path.with_suffix(".disabled")
            .is_ok_and(|p| self.entries.contains_key(&p))
    }
}

impl FromIterator<(RelPath, DiskEntry)> for DiskState {
    fn from_iter<T: IntoIterator<Item = (RelPath, DiskEntry)>>(iter: T) -> Self {
        let mut s = Self::new();
        for (p, e) in iter {
            s.insert(p, e);
        }
        s
    }
}

/// One file the resolved pack wants installed, after environment filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredFile {
    pub path: RelPath,
    /// sha512 of the intended content.
    pub content: Digest,
    pub size: Option<u64>,
    pub executable: bool,
    pub provenance: Provenance,
    pub managed: Managed,
}

/// The post-filter view of the pack: files classified as client-only or opted out are already
/// gone. That is deliberate — a previously installed mod that is now filtered out simply stops
/// being desired, so the ordinary "pack no longer wants this" path removes it with no special
/// case.
#[derive(Debug, Clone, Default)]
pub struct DesiredSet {
    files: BTreeMap<RelPath, DesiredFile>,
}

impl DesiredSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, f: DesiredFile) {
        self.files.insert(f.path.clone(), f);
    }

    pub fn get(&self, path: &RelPath) -> Option<&DesiredFile> {
        self.files.get(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &RelPath> {
        self.files.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RelPath, &DesiredFile)> {
        self.files.iter()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Two entries differing only in case would race for one filesystem slot on macOS or
    /// Windows; refusing up front beats corrupting state later.
    pub fn case_collisions(&self) -> Vec<(RelPath, RelPath)> {
        let mut seen: HashMap<String, &RelPath> = HashMap::new();
        let mut out = Vec::new();
        for p in self.files.keys() {
            match seen.entry(p.fold_key()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    out.push(((*e.get()).clone(), p.clone()));
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(p);
                }
            }
        }
        out
    }
}

impl FromIterator<DesiredFile> for DesiredSet {
    fn from_iter<T: IntoIterator<Item = DesiredFile>>(iter: T) -> Self {
        let mut s = Self::new();
        for f in iter {
            s.insert(f);
        }
        s
    }
}
