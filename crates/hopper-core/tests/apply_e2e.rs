//! End-to-end apply against a real directory.
//!
//! The unit tests prove each stage in isolation; this proves they compose. The properties that
//! matter are the ones a server operator would notice: their own files survive, mods the pack
//! dropped actually go away, their edited configs are not clobbered, and running the same
//! command twice does nothing the second time.

use std::collections::BTreeMap;
use std::path::Path;

use hopper_core::apply::{Summary, apply, next_lockfile, read_lockfile};
use hopper_core::cache::BlobStore;
use hopper_core::fs as hfs;
use hopper_core::model::{
    Digest, FileState, LOCK_VERSION, LoaderKind, LockedFile, Lockfile, Managed, MinecraftVersion,
    OverrideLayer, Provenance, RelPath,
    lock::{PackRecord, PolicyRecord, ServerRecord},
};
use hopper_core::plan::{
    ConflictPolicy, Decision, DesiredFile, DesiredSet, DiskEntry, DiskState, reconcile,
};

fn p(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn template() -> Lockfile {
    Lockfile {
        lock_version: LOCK_VERSION,
        generator: "hopper-test".into(),
        updated_at: "2026-09-15T00:00:00Z".into(),
        server: ServerRecord {
            minecraft: MinecraftVersion::new("26.3"),
            loader: LoaderKind::Fabric,
            loader_version: "0.17.2".into(),
            java_major: 25,
        },
        pack: PackRecord {
            name: "Test Pack".into(),
            version_label: Some("1.0.0".into()),
            source_arg: "test-pack".into(),
        },
        policy: PolicyRecord::default(),
        files: vec![],
        skipped: vec![],
        unknown: BTreeMap::new(),
    }
}

fn locked(path: &str, digest: Digest, size: u64) -> LockedFile {
    LockedFile {
        path: p(path),
        digest,
        size,
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

fn wanted(path: &str, content: Digest, size: u64) -> DesiredFile {
    DesiredFile {
        path: p(path),
        content,
        size: Some(size),
        executable: false,
        provenance: Provenance::Override {
            layer: OverrideLayer::Global,
        },
        managed: Managed::Full,
    }
}

/// Scan a real directory into the state reconcile expects.
fn scan(root: &Path, interesting: &[RelPath]) -> DiskState {
    let mut disk = DiskState::new();
    for path in interesting {
        let full = path.resolve_under(root);
        if let Ok((_, digest)) = hfs::hash_file(&full) {
            let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
            disk.insert(path.clone(), DiskEntry::file(digest, size));
        }
    }
    disk
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    store: BlobStore,
    txn: usize,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("server");
        let store = BlobStore::new(dir.path().join("cache"));
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            root,
            store,
            txn: 0,
        }
    }

    /// Put content in the cache and describe it as a desired file.
    fn stage(&self, path: &str, content: &[u8]) -> DesiredFile {
        let blob = self.store.insert_bytes(content, None).unwrap();
        wanted(path, blob.digest, content.len() as u64)
    }

    fn write_user_file(&self, path: &str, content: &[u8]) {
        hfs::write_atomic(&p(path).resolve_under(&self.root), content, false).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(p(path).resolve_under(&self.root)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        p(path).resolve_under(&self.root).exists()
    }

    /// Run one full resolve-plan-apply cycle.
    fn install(&mut self, desired: &DesiredSet) -> (Summary, Vec<(RelPath, Decision)>) {
        let lock = read_lockfile(&self.root).unwrap();

        let mut interesting: Vec<RelPath> = desired.paths().cloned().collect();
        if let Some(l) = &lock {
            interesting.extend(l.files.iter().map(|f| f.path.clone()));
        }
        let disk = scan(&self.root, &interesting);

        let decisions = reconcile(lock.as_ref(), &disk, desired, &ConflictPolicy::default());
        let summary = Summary::of(&decisions);

        let entries: BTreeMap<RelPath, LockedFile> = desired
            .iter()
            .map(|(path, f)| {
                (
                    path.clone(),
                    locked(path.as_str(), f.content.clone(), f.size.unwrap_or(0)),
                )
            })
            .collect();
        let next = next_lockfile(lock.as_ref(), &decisions, &entries, template());

        self.txn += 1;
        apply(
            &self.root,
            &decisions,
            &next,
            &self.store,
            &format!("txn-{}", self.txn),
            "hopper-test",
            "2026-09-15T00:00:00Z",
        )
        .unwrap();

        (summary, decisions)
    }
}

#[test]
fn installs_a_pack_into_an_empty_directory() {
    let mut f = Fixture::new();
    let desired: DesiredSet = [
        f.stage("mods/sodium.jar", b"sodium v1"),
        f.stage("mods/lithium.jar", b"lithium v1"),
        f.stage("config/sodium.properties", b"quality=high"),
    ]
    .into_iter()
    .collect();

    let (summary, _) = f.install(&desired);
    assert_eq!(summary.added, 3);

    assert_eq!(f.read("mods/sodium.jar").unwrap(), b"sodium v1");
    assert_eq!(f.read("config/sodium.properties").unwrap(), b"quality=high");

    // The transaction closed cleanly.
    assert!(f.exists(".hopper/lock.json"));
    assert!(!f.exists(".hopper/journal.json"), "journal must be removed");
}

#[test]
fn running_the_same_install_twice_changes_nothing() {
    // Convergence: the property that catches most reconcile bugs.
    let mut f = Fixture::new();
    let desired: DesiredSet = [
        f.stage("mods/a.jar", b"a"),
        f.stage("config/a.toml", b"x=1"),
    ]
    .into_iter()
    .collect();

    let (first, _) = f.install(&desired);
    assert!(first.changes_anything());

    let (second, decisions) = f.install(&desired);
    assert!(
        !second.changes_anything(),
        "a settled directory must plan no writes: {second:?}"
    );
    assert!(decisions.iter().all(|(_, d)| !d.mutates()));
}

#[test]
fn an_update_upgrades_adds_and_removes_in_one_pass() {
    let mut f = Fixture::new();
    let v1: DesiredSet = [
        f.stage("mods/keep.jar", b"keep"),
        f.stage("mods/bump.jar", b"bump v1"),
        f.stage("mods/drop.jar", b"drop"),
    ]
    .into_iter()
    .collect();
    f.install(&v1);

    let v2: DesiredSet = [
        f.stage("mods/keep.jar", b"keep"),
        f.stage("mods/bump.jar", b"bump v2"),
        f.stage("mods/new.jar", b"new"),
    ]
    .into_iter()
    .collect();
    let (summary, _) = f.install(&v2);

    assert_eq!(summary.upgraded, 1);
    assert_eq!(summary.added, 1);
    assert_eq!(summary.removed, 1);
    assert_eq!(summary.unchanged, 1);

    assert_eq!(f.read("mods/bump.jar").unwrap(), b"bump v2");
    assert_eq!(f.read("mods/new.jar").unwrap(), b"new");
    assert!(
        !f.exists("mods/drop.jar"),
        "a mod the pack dropped must actually go away"
    );
}

#[test]
fn files_the_operator_added_are_never_touched() {
    // The headline promise, on a real directory.
    let mut f = Fixture::new();
    let v1: DesiredSet = [f.stage("mods/a.jar", b"a")].into_iter().collect();
    f.install(&v1);

    f.write_user_file("mods/my-plugin.jar", b"mine");
    f.write_user_file("config/my-notes.txt", b"personal");

    // A later update that removes everything the pack shipped.
    let v2 = DesiredSet::new();
    f.install(&v2);

    assert!(!f.exists("mods/a.jar"), "the pack's own file goes");
    assert_eq!(f.read("mods/my-plugin.jar").unwrap(), b"mine");
    assert_eq!(f.read("config/my-notes.txt").unwrap(), b"personal");
}

#[test]
fn an_edited_config_survives_an_update_and_the_packs_version_lands_beside_it() {
    let mut f = Fixture::new();
    let v1: DesiredSet = [f.stage("config/mod.toml", b"setting=default")]
        .into_iter()
        .collect();
    f.install(&v1);

    // The operator tunes it.
    f.write_user_file("config/mod.toml", b"setting=tuned-by-me");

    // The pack ships a new default.
    let v2: DesiredSet = [f.stage("config/mod.toml", b"setting=new-default")]
        .into_iter()
        .collect();
    let (summary, _) = f.install(&v2);

    assert_eq!(summary.conflicts, 1);
    assert_eq!(
        f.read("config/mod.toml").unwrap(),
        b"setting=tuned-by-me",
        "their edit must win"
    );
    assert_eq!(
        f.read("config/mod.toml.new").unwrap(),
        b"setting=new-default",
        "and the pack's version should be available to compare"
    );
}

#[test]
fn an_edited_config_the_pack_did_not_change_is_preserved_quietly() {
    let mut f = Fixture::new();
    let desired: DesiredSet = [f.stage("config/mod.toml", b"setting=default")]
        .into_iter()
        .collect();
    f.install(&desired);

    f.write_user_file("config/mod.toml", b"setting=mine");

    let (summary, _) = f.install(&desired);
    assert_eq!(summary.preserved, 1);
    assert_eq!(f.read("config/mod.toml").unwrap(), b"setting=mine");
    assert!(
        !f.exists("config/mod.toml.new"),
        "nothing changed upstream, so there is nothing to compare against"
    );

    // Second time round it is recorded, so it stops being reported.
    let (again, decisions) = f.install(&desired);
    assert_eq!(again.preserved, 1);
    assert!(
        decisions.iter().all(|(_, d)| !d.is_noteworthy()),
        "an acknowledged edit should not be reported again"
    );
}

#[test]
fn installing_into_a_directory_that_already_has_content_adopts_rather_than_clobbers() {
    let mut f = Fixture::new();
    // Exactly what the pack wants, already present.
    f.write_user_file("mods/same.jar", b"identical");
    // Something else at a path the pack wants.
    f.write_user_file("mods/different.jar", b"theirs");

    let desired: DesiredSet = [
        f.stage("mods/same.jar", b"identical"),
        f.stage("mods/different.jar", b"the pack's version"),
    ]
    .into_iter()
    .collect();

    let (summary, _) = f.install(&desired);
    assert_eq!(
        summary.adopted, 1,
        "identical content is taken over silently"
    );
    assert_eq!(summary.conflicts, 1);
    assert_eq!(
        f.read("mods/different.jar").unwrap(),
        b"theirs",
        "a file we did not install must not be overwritten"
    );
}

#[test]
fn a_mod_deleted_by_the_operator_is_restored() {
    let mut f = Fixture::new();
    let desired: DesiredSet = [f.stage("mods/a.jar", b"a")].into_iter().collect();
    f.install(&desired);

    std::fs::remove_file(p("mods/a.jar").resolve_under(&f.root)).unwrap();

    let (summary, _) = f.install(&desired);
    assert_eq!(summary.restored, 1);
    assert_eq!(f.read("mods/a.jar").unwrap(), b"a");
}

#[test]
fn a_directory_emptied_by_removals_is_pruned() {
    let mut f = Fixture::new();
    let v1: DesiredSet = [f.stage("config/deep/nested/a.toml", b"a")]
        .into_iter()
        .collect();
    f.install(&v1);
    assert!(f.exists("config/deep/nested"));

    f.install(&DesiredSet::new());
    assert!(
        !f.exists("config/deep/nested"),
        "empty directories should not accumulate"
    );
}

#[test]
fn a_directory_still_holding_operator_files_is_not_pruned() {
    let mut f = Fixture::new();
    let v1: DesiredSet = [f.stage("config/shared/a.toml", b"a")]
        .into_iter()
        .collect();
    f.install(&v1);
    f.write_user_file("config/shared/mine.toml", b"mine");

    f.install(&DesiredSet::new());
    assert!(f.exists("config/shared"), "their file keeps the directory");
    assert_eq!(f.read("config/shared/mine.toml").unwrap(), b"mine");
}

#[test]
fn the_lockfile_reflects_exactly_what_is_on_disk() {
    let mut f = Fixture::new();
    let desired: DesiredSet = [f.stage("mods/a.jar", b"a"), f.stage("mods/b.jar", b"b")]
        .into_iter()
        .collect();
    f.install(&desired);

    let lock = read_lockfile(&f.root).unwrap().unwrap();
    assert_eq!(lock.files.len(), 2);
    for entry in &lock.files {
        let full = entry.path.resolve_under(&f.root);
        let (_, actual) = hfs::hash_file(&full).unwrap();
        assert_eq!(actual, entry.digest, "{} drifted", entry.path);
    }
}

#[test]
fn a_pack_removed_file_that_the_operator_edited_is_left_behind_not_deleted() {
    let mut f = Fixture::new();
    let v1: DesiredSet = [f.stage("config/a.toml", b"original")]
        .into_iter()
        .collect();
    f.install(&v1);

    f.write_user_file("config/a.toml", b"edited by me");

    f.install(&DesiredSet::new());
    assert_eq!(
        f.read("config/a.toml").unwrap(),
        b"edited by me",
        "their work outranks our tidiness"
    );
    let lock = read_lockfile(&f.root).unwrap().unwrap();
    assert!(lock.files.is_empty(), "but we stop claiming to manage it");
}
