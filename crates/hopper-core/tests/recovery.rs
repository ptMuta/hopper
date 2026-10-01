//! Crash recovery.
//!
//! The property that matters is convergence: after an interruption at any point, the next run
//! reaches the same state a clean run would have, and the run after that is a no-op. These
//! tests simulate the crash by constructing the intermediate state directly, which is both
//! deterministic and far faster than actually killing a process.

use std::collections::BTreeMap;

use hopper_core::apply::{Intent, Journal, Recovery, Removal, recover};
use hopper_core::model::{
    Digest, FileState, HashAlgo, LOCK_VERSION, LoaderKind, LockedFile, Lockfile, Managed,
    MinecraftVersion, ProjectId, Provenance, RegistryId, RelPath, VersionId,
    lock::{PackRecord, PolicyRecord, ServerRecord},
};
use hopper_core::plan::{
    ConflictPolicy, Decision, DesiredFile, DesiredSet, DiskEntry, DiskState, reconcile,
};

fn d(seed: u8) -> Digest {
    Digest::new(HashAlgo::Sha512, &format!("{seed:0128x}")).unwrap()
}

fn p(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn provenance() -> Provenance {
    Provenance::Registry {
        registry: RegistryId::Modrinth,
        project: ProjectId::new("AANobbMI"),
        version: VersionId::new("v1"),
        slug: None,
        version_number: None,
    }
}

fn locked(path: &str, digest: Digest) -> LockedFile {
    LockedFile {
        path: p(path),
        digest,
        size: 100,
        state: FileState::Managed,
        provenance: provenance(),
        managed: Managed::Full,
        executable: false,
        user_modified: None,
        disabled: false,
        mtime_ns: None,
    }
}

fn wanted(path: &str, content: Digest) -> DesiredFile {
    DesiredFile {
        path: p(path),
        content,
        size: Some(100),
        executable: false,
        provenance: provenance(),
        managed: Managed::Full,
    }
}

fn lockfile(files: Vec<LockedFile>) -> Lockfile {
    let mut files = files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Lockfile {
        lock_version: LOCK_VERSION,
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
            name: "Test".into(),
            version_label: None,
            source_arg: "test".into(),
            ..Default::default()
        },
        policy: PolicyRecord::default(),
        files,
        skipped: vec![],
        unknown: BTreeMap::new(),
    }
}

fn journal_for(next: Lockfile, intended: &[(&str, u8)], removing: &[(&str, u8)]) -> Journal {
    let mut j = Journal::new("txn-1", "2026-09-15T00:00:00Z", "hopper-test", next);
    j.intended = intended
        .iter()
        .map(|(path, seed)| Intent {
            path: p(path),
            digest: d(*seed),
            executable: false,
        })
        .collect();
    j.removing = removing
        .iter()
        .map(|(path, seed)| Removal {
            path: p(path),
            digest: d(*seed),
        })
        .collect();
    j
}

// ---------------------------------------------------------------- the recovery rule

#[test]
fn no_journal_means_nothing_to_recover() {
    assert_eq!(recover(None, None, &BTreeMap::new()), Recovery::Nothing);
}

#[test]
fn a_file_we_wrote_but_never_recorded_is_reclaimed() {
    // The dangerous window: written in step 3, killed before step 5. Without this, the file
    // would look operator-added forever and could never be upgraded or cleaned up.
    let next = lockfile(vec![locked("mods/new.jar", d(1))]);
    let j = journal_for(next, &[("mods/new.jar", 1)], &[]);

    let disk: BTreeMap<RelPath, Digest> = [(p("mods/new.jar"), d(1))].into_iter().collect();
    let Recovery::Resumed { adopted } = recover(Some(j.to_json().unwrap().as_bytes()), None, &disk)
    else {
        panic!("should resume");
    };
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].path.as_str(), "mods/new.jar");
    assert_eq!(adopted[0].digest, d(1));
}

#[test]
fn a_file_with_different_content_is_not_claimed() {
    // We intended to write `mods/x.jar`, but what is there is not what we were writing. It is
    // the operator's, and claiming it would let a later run delete their file.
    let next = lockfile(vec![locked("mods/x.jar", d(1))]);
    let j = journal_for(next, &[("mods/x.jar", 1)], &[]);

    let disk: BTreeMap<RelPath, Digest> = [(p("mods/x.jar"), d(99))].into_iter().collect();
    let Recovery::Resumed { adopted } = recover(Some(j.to_json().unwrap().as_bytes()), None, &disk)
    else {
        panic!("should resume");
    };
    assert!(
        adopted.is_empty(),
        "content must match before we claim a file"
    );
}

#[test]
fn files_the_operator_added_during_the_crash_window_stay_theirs() {
    let next = lockfile(vec![locked("mods/ours.jar", d(1))]);
    let j = journal_for(next, &[("mods/ours.jar", 1)], &[]);

    let disk: BTreeMap<RelPath, Digest> =
        [(p("mods/ours.jar"), d(1)), (p("mods/theirs.jar"), d(50))]
            .into_iter()
            .collect();
    let Recovery::Resumed { adopted } = recover(Some(j.to_json().unwrap().as_bytes()), None, &disk)
    else {
        panic!("should resume");
    };
    let paths: Vec<&str> = adopted.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["mods/ours.jar"]);
}

#[test]
fn already_committed_files_are_not_re_adopted() {
    let committed = lockfile(vec![locked("mods/a.jar", d(1))]);
    let next = lockfile(vec![locked("mods/a.jar", d(2))]);
    let j = journal_for(next, &[("mods/a.jar", 2)], &[]);

    let disk: BTreeMap<RelPath, Digest> = [(p("mods/a.jar"), d(2))].into_iter().collect();
    let Recovery::Resumed { adopted } = recover(
        Some(j.to_json().unwrap().as_bytes()),
        Some(&committed),
        &disk,
    ) else {
        panic!("should resume");
    };
    assert!(
        adopted.is_empty(),
        "tracked files are handled by ordinary reconcile, not recovery"
    );
}

#[test]
fn an_unreadable_journal_asks_the_operator_rather_than_guessing() {
    // Could be a truncated write from the crash itself, or a newer hopper. Either way,
    // improvising on top of an unknown half-applied state is worse than stopping.
    let got = recover(Some(b"{ truncated"), None, &BTreeMap::new());
    assert!(matches!(got, Recovery::NeedsOperator { .. }), "got {got:?}");
}

#[test]
fn a_journal_from_a_newer_hopper_is_refused_with_advice() {
    let next = lockfile(vec![]);
    let j = journal_for(next, &[], &[]);
    let mut v: serde_json::Value = serde_json::from_str(&j.to_json().unwrap()).unwrap();
    v["journal_version"] = serde_json::json!(99);

    let got = recover(Some(v.to_string().as_bytes()), None, &BTreeMap::new());
    let Recovery::NeedsOperator { reason } = got else {
        panic!("should need an operator");
    };
    assert!(reason.contains("upgrade hopper"), "got {reason}");
}

// ---------------------------------------------------------------- convergence

/// Reconcile after recovery, the way a real re-run would.
fn plan_after_recovery(
    committed: Option<&Lockfile>,
    journal: Option<&Journal>,
    disk_digests: &BTreeMap<RelPath, Digest>,
    desired: &DesiredSet,
) -> Vec<(RelPath, Decision)> {
    // Fold anything recovery reclaimed into the lockfile we reconcile against.
    let mut effective = committed.cloned().unwrap_or_else(|| lockfile(vec![]));
    if let Some(j) = journal {
        for entry in j.adopt_orphans(committed, disk_digests.iter()) {
            effective.files.push(entry);
        }
        effective.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    let disk: DiskState = disk_digests
        .iter()
        .map(|(path, dig)| (path.clone(), DiskEntry::file(dig.clone(), 100)))
        .collect();
    reconcile(Some(&effective), &disk, desired, &ConflictPolicy::default())
}

#[test]
fn an_interrupted_install_converges_and_then_becomes_a_noop() {
    // A three-file install killed after two files landed.
    let next = lockfile(vec![
        locked("mods/a.jar", d(1)),
        locked("mods/b.jar", d(2)),
        locked("mods/c.jar", d(3)),
    ]);
    let j = journal_for(
        next,
        &[("mods/a.jar", 1), ("mods/b.jar", 2), ("mods/c.jar", 3)],
        &[],
    );
    let desired: DesiredSet = [
        wanted("mods/a.jar", d(1)),
        wanted("mods/b.jar", d(2)),
        wanted("mods/c.jar", d(3)),
    ]
    .into_iter()
    .collect();

    // Crash state: a and b written, c never made it, lockfile never committed.
    let mut disk: BTreeMap<RelPath, Digest> = [(p("mods/a.jar"), d(1)), (p("mods/b.jar"), d(2))]
        .into_iter()
        .collect();

    let out = plan_after_recovery(None, Some(&j), &disk, &desired);
    let by: BTreeMap<_, _> = out.iter().map(|(k, v)| (k.as_str(), v)).collect();

    // The two that landed are recognised as ours, not re-downloaded and not conflicted.
    assert!(
        matches!(by["mods/a.jar"], Decision::Keep { .. }),
        "{:?}",
        by["mods/a.jar"]
    );
    assert!(matches!(by["mods/b.jar"], Decision::Keep { .. }));
    // The missing one is simply written.
    assert!(matches!(by["mods/c.jar"], Decision::Add { .. }));
    for (_, decision) in &out {
        assert!(
            !matches!(
                decision,
                Decision::Remove { .. } | Decision::Conflict { .. }
            ),
            "recovery must not destroy or conflict: {decision:?}"
        );
    }

    // Finish the job, then assert the next run has nothing to do.
    disk.insert(p("mods/c.jar"), d(3));
    let committed = lockfile(vec![
        locked("mods/a.jar", d(1)),
        locked("mods/b.jar", d(2)),
        locked("mods/c.jar", d(3)),
    ]);
    let again = plan_after_recovery(Some(&committed), None, &disk, &desired);
    assert!(
        again.iter().all(|(_, d)| !d.mutates()),
        "a settled directory must plan no writes: {again:?}"
    );
}

#[test]
fn without_the_journal_a_half_written_file_would_be_stranded() {
    // The counterfactual that justifies the journal existing at all. Same crash state, but
    // reconcile with no recovery input: the file we wrote looks like the operator's, so it is
    // untracked, and a pack that later drops it could never clean it up.
    let desired: DesiredSet = [wanted("mods/a.jar", d(1))].into_iter().collect();
    let disk: BTreeMap<RelPath, Digest> = [(p("mods/a.jar"), d(1))].into_iter().collect();

    let out = plan_after_recovery(None, None, &disk, &desired);
    let (_, decision) = &out[0];
    assert!(
        matches!(decision, Decision::Adopt { .. }),
        "without the journal this is merely adopted, never reclaimed as Managed: {decision:?}"
    );

    // With the journal, the same state is reclaimed with full ownership.
    let next = lockfile(vec![locked("mods/a.jar", d(1))]);
    let j = journal_for(next, &[("mods/a.jar", 1)], &[]);
    let out = plan_after_recovery(None, Some(&j), &disk, &desired);
    assert!(matches!(out[0].1, Decision::Keep { .. }));
}

#[test]
fn an_interrupted_removal_finishes_on_the_next_run() {
    // Killed after the lockfile still lists a file the new pack drops; the deletion simply
    // happens next time, because reconcile does not care when it was decided.
    let committed = lockfile(vec![locked("mods/old.jar", d(1))]);
    let desired = DesiredSet::new();
    let disk: BTreeMap<RelPath, Digest> = [(p("mods/old.jar"), d(1))].into_iter().collect();

    let out = plan_after_recovery(Some(&committed), None, &disk, &desired);
    assert_eq!(out[0].1, Decision::Remove { was: d(1) });
}

#[test]
fn recovery_is_reentrant() {
    // A crash during recovery leaves exactly the same evidence, so recovering twice is
    // indistinguishable from recovering once.
    let next = lockfile(vec![locked("mods/a.jar", d(1))]);
    let j = journal_for(next, &[("mods/a.jar", 1)], &[]);
    let disk: BTreeMap<RelPath, Digest> = [(p("mods/a.jar"), d(1))].into_iter().collect();

    let bytes = j.to_json().unwrap();
    let first = recover(Some(bytes.as_bytes()), None, &disk);
    let second = recover(Some(bytes.as_bytes()), None, &disk);
    assert_eq!(first, second);
}

#[test]
fn journal_round_trips() {
    let next = lockfile(vec![locked("mods/a.jar", d(1))]);
    let j = journal_for(next, &[("mods/a.jar", 1)], &[("mods/b.jar", 2)]);
    let back = Journal::load(j.to_json().unwrap().as_bytes()).unwrap();
    assert_eq!(back, j);
}
