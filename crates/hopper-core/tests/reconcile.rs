//! The reconcile decision matrix, exhaustively.
//!
//! Two invariants matter more than any individual row, and they are asserted separately at the
//! bottom over every reachable combination:
//!
//! 1. A file hopper did not install is never deleted.
//! 2. A file the operator edited is never silently overwritten.

use std::collections::BTreeMap;

use hopper_core::model::{
    Digest, FileState, HashAlgo, LOCK_VERSION, LoaderKind, LockedFile, Lockfile, Managed,
    MinecraftVersion, OverrideLayer, ProjectId, Provenance, RegistryId, RelPath, VersionId,
    lock::{GeneratedKind, PackRecord, PolicyRecord, ServerRecord},
};
use hopper_core::plan::{
    ConflictPolicy, ConflictReason, ConflictResolution, Decision, DesiredFile, DesiredSet,
    DiskEntry, DiskState, EntryKind, RejectReason, Triple, UntrackReason, classify, is_protected,
    reconcile, reconcile::Hints,
};

// ---------------------------------------------------------------- helpers

/// Distinct, deterministic digests. `d(1)` and `d(2)` are different files.
fn d(seed: u8) -> Digest {
    Digest::new(HashAlgo::Sha512, &format!("{seed:0128x}")).unwrap()
}

fn p(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn jar_provenance() -> Provenance {
    Provenance::Registry {
        registry: RegistryId::Modrinth,
        project: ProjectId::new("AANobbMI"),
        version: VersionId::new("SMxNOGZ6"),
        slug: Some("sodium".into()),
        version_number: Some("0.8.13".into()),
    }
}

fn config_provenance() -> Provenance {
    Provenance::Override {
        layer: OverrideLayer::Global,
    }
}

fn locked(path: &str, digest: Digest) -> LockedFile {
    LockedFile {
        path: p(path),
        digest,
        size: 100,
        state: FileState::Managed,
        provenance: jar_provenance(),
        managed: Managed::Full,
        executable: false,
        user_modified: None,
        disabled: false,
        mtime_ns: None,
    }
}

fn on_disk(digest: Digest) -> DiskEntry {
    DiskEntry::file(digest, 100)
}

fn wanted(path: &str, content: Digest) -> DesiredFile {
    DesiredFile {
        path: p(path),
        content,
        size: Some(100),
        executable: false,
        provenance: jar_provenance(),
        managed: Managed::Full,
    }
}

fn decide(
    lock: Option<&LockedFile>,
    disk: Option<&DiskEntry>,
    want: Option<&DesiredFile>,
) -> Decision {
    decide_with(lock, disk, want, &ConflictPolicy::default())
}

fn decide_with(
    lock: Option<&LockedFile>,
    disk: Option<&DiskEntry>,
    want: Option<&DesiredFile>,
    pol: &ConflictPolicy,
) -> Decision {
    let path = p("mods/a.jar");
    classify(
        &Triple {
            path: &path,
            lock,
            disk,
            want,
            hints: Hints::default(),
        },
        pol,
    )
}

fn decide_hinted(
    lock: Option<&LockedFile>,
    disk: Option<&DiskEntry>,
    want: Option<&DesiredFile>,
    hints: Hints,
) -> Decision {
    let path = p("mods/a.jar");
    classify(
        &Triple {
            path: &path,
            lock,
            disk,
            want,
            hints,
        },
        &ConflictPolicy::default(),
    )
}

fn lockfile_with(files: Vec<LockedFile>) -> Lockfile {
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

// ---------------------------------------------------------------- the matrix

#[test]
fn fresh_install_adds() {
    let w = wanted("mods/a.jar", d(1));
    assert_eq!(
        decide(None, None, Some(&w)),
        Decision::Add {
            content: d(1),
            restore: false
        }
    );
}

#[test]
fn untracked_file_the_pack_does_not_want_is_left_alone() {
    // The headline promise. An operator's hand-dropped jar is invisible to us.
    assert_eq!(
        decide(None, Some(&on_disk(d(9))), None),
        Decision::LeaveAlone
    );
}

#[test]
fn untracked_file_matching_the_pack_is_adopted_without_writing() {
    let w = wanted("mods/a.jar", d(1));
    assert_eq!(
        decide(None, Some(&on_disk(d(1))), Some(&w)),
        Decision::Adopt {
            content: d(1),
            reconciled: false
        }
    );
}

#[test]
fn untracked_file_differing_from_the_pack_is_a_conflict_not_an_overwrite() {
    let w = wanted("mods/a.jar", d(1));
    let got = decide(None, Some(&on_disk(d(2))), Some(&w));
    assert_eq!(
        got,
        Decision::Conflict {
            reason: ConflictReason::UntrackedCollision,
            resolution: ConflictResolution::Skip,
            desired: Some(d(1)),
            on_disk: Some(d(2)),
        }
    );
    assert!(!got.mutates(), "must not touch a file we do not own");
}

#[test]
fn adopt_collisions_opt_in_backs_up_first() {
    let w = wanted("mods/a.jar", d(1));
    let pol = ConflictPolicy {
        adopt_collisions: true,
        ..Default::default()
    };
    assert_eq!(
        decide_with(None, Some(&on_disk(d(2))), Some(&w), &pol),
        Decision::Conflict {
            reason: ConflictReason::UntrackedCollision,
            resolution: ConflictResolution::BackupThenWrite,
            desired: Some(d(1)),
            on_disk: Some(d(2)),
        }
    );
}

#[test]
fn tracked_and_gone_and_unwanted_is_forgotten() {
    let l = locked("mods/a.jar", d(1));
    assert_eq!(
        decide(Some(&l), None, None),
        Decision::Untrack {
            reason: UntrackReason::AlreadyGone
        }
    );
}

#[test]
fn tracked_and_gone_but_still_wanted_is_restored() {
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(1));
    assert_eq!(
        decide(Some(&l), None, Some(&w)),
        Decision::Add {
            content: d(1),
            restore: true
        }
    );
}

#[test]
fn restore_can_be_declined() {
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(1));
    let pol = ConflictPolicy {
        restore_deleted: false,
        ..Default::default()
    };
    assert_eq!(
        decide_with(Some(&l), None, Some(&w), &pol),
        Decision::Untrack {
            reason: UntrackReason::UserDeleted
        }
    );
}

#[test]
fn removed_from_pack_and_unmodified_is_deleted() {
    // The orphan cleanup Prism never does: a mod the pack dropped actually goes away.
    let l = locked("mods/a.jar", d(1));
    assert_eq!(
        decide(Some(&l), Some(&on_disk(d(1))), None),
        Decision::Remove { was: d(1) }
    );
}

#[test]
fn removed_from_pack_but_locally_modified_is_left_behind() {
    let l = locked("mods/a.jar", d(1));
    let got = decide(Some(&l), Some(&on_disk(d(2))), None);
    assert_eq!(
        got,
        Decision::Untrack {
            reason: UntrackReason::RemovedButModified
        }
    );
    assert!(!got.mutates(), "their edit outranks our tidiness");
}

#[test]
fn unchanged_file_is_kept_quietly() {
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(1));
    let got = decide(Some(&l), Some(&on_disk(d(1))), Some(&w));
    assert_eq!(
        got,
        Decision::Keep {
            digest: d(1),
            disabled: false
        }
    );
    assert!(!got.is_noteworthy(), "should fold into a count, not a line");
}

#[test]
fn clean_upgrade_replaces() {
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(2));
    assert_eq!(
        decide(Some(&l), Some(&on_disk(d(1))), Some(&w)),
        Decision::Replace {
            from: d(1),
            to: d(2)
        }
    );
}

#[test]
fn hand_applied_update_only_fixes_the_bookkeeping() {
    // Disk already holds the new version: no download, no write.
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(2));
    let got = decide(Some(&l), Some(&on_disk(d(2))), Some(&w));
    assert_eq!(
        got,
        Decision::Adopt {
            content: d(2),
            reconciled: true
        }
    );
    assert!(!got.mutates());
}

#[test]
fn locally_modified_with_unchanged_pack_is_preserved_and_reported_once() {
    let l = locked("config/x.toml", d(1));
    let w = DesiredFile {
        provenance: config_provenance(),
        ..wanted("config/x.toml", d(1))
    };
    let got = decide(Some(&l), Some(&on_disk(d(5))), Some(&w));
    assert_eq!(
        got,
        Decision::Preserve {
            lock: d(1),
            on_disk: d(5),
            first_seen: true
        }
    );
    assert!(got.is_noteworthy(), "the first sighting should be reported");
    assert!(!got.mutates());
}

#[test]
fn an_acknowledged_edit_stops_being_reported() {
    // Second run over the same edit: still preserved, but no longer nagged about.
    let mut l = locked("config/x.toml", d(1));
    l.user_modified = Some(d(5));
    let w = wanted("config/x.toml", d(1));
    let got = decide(Some(&l), Some(&on_disk(d(5))), Some(&w));
    assert_eq!(
        got,
        Decision::Preserve {
            lock: d(1),
            on_disk: d(5),
            first_seen: false
        }
    );
    assert!(!got.is_noteworthy());
}

#[test]
fn modified_and_upgraded_is_a_real_conflict() {
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(2));
    assert_eq!(
        decide(Some(&l), Some(&on_disk(d(3))), Some(&w)),
        Decision::Conflict {
            reason: ConflictReason::LocalModification,
            resolution: ConflictResolution::BackupThenWrite,
            desired: Some(d(2)),
            on_disk: Some(d(3)),
        }
    );
}

#[test]
fn conflict_resolution_follows_provenance_not_path_shape() {
    let l = locked("mods/a.jar", d(1));
    let disk = on_disk(d(3));

    // An operator-tuned config: theirs wins, ours lands as `.new`.
    let cfg = DesiredFile {
        provenance: config_provenance(),
        ..wanted("mods/a.jar", d(2))
    };
    assert!(matches!(
        decide(Some(&l), Some(&disk), Some(&cfg)),
        Decision::Conflict {
            resolution: ConflictResolution::KeepOursWriteNew,
            ..
        }
    ));

    // A mod jar: the pack must win, but keep a copy of their patched jar.
    let jar = wanted("mods/a.jar", d(2));
    assert!(matches!(
        decide(Some(&l), Some(&disk), Some(&jar)),
        Decision::Conflict {
            resolution: ConflictResolution::BackupThenWrite,
            ..
        }
    ));

    // Loader output is machine-generated; nobody hand-edits it.
    let lib = DesiredFile {
        provenance: Provenance::Loader {
            loader: LoaderKind::NeoForge,
            version: "21.1.95".into(),
        },
        ..wanted("mods/a.jar", d(2))
    };
    assert!(matches!(
        decide(Some(&l), Some(&disk), Some(&lib)),
        Decision::Conflict {
            resolution: ConflictResolution::Overwrite,
            ..
        }
    ));
}

#[test]
fn seed_files_are_written_once_and_then_never_touched() {
    // eula.txt and server.properties belong to the operator after the first write.
    let mut l = locked("mods/a.jar", d(1));
    l.managed = Managed::ContentSeed;
    let w = DesiredFile {
        managed: Managed::ContentSeed,
        provenance: Provenance::Generated {
            kind: GeneratedKind::ServerProperties,
        },
        ..wanted("mods/a.jar", d(2))
    };
    // Changed by the operator, pack ships something else: keep theirs.
    let got = decide(Some(&l), Some(&on_disk(d(7))), Some(&w));
    assert!(matches!(got, Decision::Keep { .. }), "got {got:?}");
    assert!(!got.mutates());

    // And it is never deleted when the pack stops shipping it.
    let got = decide(Some(&l), Some(&on_disk(d(1))), None);
    assert!(matches!(got, Decision::Keep { .. }), "got {got:?}");
    assert!(!got.mutates());
}

// ---------------------------------------------------------------- adopted files

#[test]
fn an_adopted_file_is_never_deleted_when_the_pack_drops_it() {
    // This is why `Adopted` exists: it was the operator's file before the pack wanted it.
    let mut l = locked("config/x.toml", d(1));
    l.state = FileState::Adopted;
    let got = decide(Some(&l), Some(&on_disk(d(1))), None);
    assert_eq!(
        got,
        Decision::Untrack {
            reason: UntrackReason::WasAdopted
        }
    );
    assert!(!got.mutates());
}

#[test]
fn an_adopted_file_the_operator_edited_is_not_overwritten() {
    let mut l = locked("config/x.toml", d(1));
    l.state = FileState::Adopted;
    let w = wanted("config/x.toml", d(2));
    let got = decide(Some(&l), Some(&on_disk(d(3))), Some(&w));
    assert!(matches!(got, Decision::Conflict { .. }), "got {got:?}");
    assert!(!got.mutates());
}

// ---------------------------------------------------------------- guards

#[test]
fn a_directory_in_the_way_is_refused_rather_than_removed() {
    let w = wanted("mods/a.jar", d(1));
    let dir = DiskEntry {
        kind: EntryKind::Dir,
        digest: None,
        size: 0,
        executable: false,
        mtime_ns: None,
    };
    assert_eq!(
        decide(None, Some(&dir), Some(&w)),
        Decision::Reject {
            reason: RejectReason::DirectoryInTheWay
        }
    );
}

#[test]
fn a_symlink_is_never_written_through_even_when_tracked() {
    // Operators symlink mods/ at shared storage; writing through would escape our directory.
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(2));
    let link = DiskEntry {
        kind: EntryKind::Symlink,
        digest: Some(d(1)),
        size: 0,
        executable: false,
        mtime_ns: None,
    };
    assert_eq!(
        decide(Some(&l), Some(&link), Some(&w)),
        Decision::Reject {
            reason: RejectReason::SymlinkInTheWay
        }
    );
}

#[test]
fn protected_paths_are_inert() {
    let l = locked("mods/a.jar", d(1));
    let got = decide_hinted(
        Some(&l),
        Some(&on_disk(d(1))),
        None,
        Hints {
            disabled_marker: false,
            protected: true,
        },
    );
    assert_eq!(
        got,
        Decision::Reject {
            reason: RejectReason::ProtectedPath
        }
    );
    assert!(!got.mutates(), "a modpack update must never touch a world");
}

#[test]
fn protection_covers_worlds_logs_and_ban_lists() {
    for path in [
        "world/level.dat",
        "world_nether/region/r.0.0.mca",
        "world_the_end/level.dat",
        "logs/latest.log",
        "crash-reports/crash.txt",
        "ops.json",
        "banned-players.json",
        "usercache.json",
        ".hopper/lock.json",
    ] {
        assert!(is_protected(&p(path)), "{path} should be protected");
    }
    for path in ["mods/a.jar", "config/x.toml", "server.properties"] {
        assert!(!is_protected(&p(path)), "{path} should not be protected");
    }
}

#[test]
fn a_disabled_mod_stays_disabled_across_updates() {
    // Renaming to `.disabled` is the manual override; an update must not silently undo it.
    let l = locked("mods/a.jar", d(1));
    let w = wanted("mods/a.jar", d(2));
    let got = decide_hinted(
        Some(&l),
        None,
        Some(&w),
        Hints {
            disabled_marker: true,
            protected: false,
        },
    );
    assert_eq!(
        got,
        Decision::Keep {
            digest: d(1),
            disabled: true
        }
    );
    assert!(!got.mutates());
}

// ---------------------------------------------------------------- the two guarantees

/// Every reachable combination of lockfile / disk / desired state.
fn every_combination() -> Vec<(Option<LockedFile>, Option<DiskEntry>, Option<DesiredFile>)> {
    let mut locks: Vec<Option<LockedFile>> = vec![None];
    for state in [FileState::Managed, FileState::Adopted] {
        for edit in [None, Some(d(5))] {
            for managed in [Managed::Full, Managed::ContentSeed] {
                let mut l = locked("mods/a.jar", d(1));
                l.state = state;
                l.user_modified = edit.clone();
                l.managed = managed;
                locks.push(Some(l));
            }
        }
    }
    let disks: Vec<Option<DiskEntry>> = vec![
        None,
        Some(on_disk(d(1))), // our bytes
        Some(on_disk(d(2))), // the pack's new bytes
        Some(on_disk(d(5))), // an acknowledged edit
        Some(on_disk(d(7))), // an unknown edit
    ];
    let wants: Vec<Option<DesiredFile>> = vec![
        None,
        Some(wanted("mods/a.jar", d(1))),
        Some(wanted("mods/a.jar", d(2))),
    ];

    let mut out = Vec::new();
    for l in &locks {
        for k in &disks {
            for w in &wants {
                out.push((l.clone(), k.clone(), w.clone()));
            }
        }
    }
    out
}

#[test]
fn guarantee_never_deletes_a_file_hopper_did_not_install() {
    let mut removes = 0;
    for (l, k, w) in every_combination() {
        let got = decide(l.as_ref(), k.as_ref(), w.as_ref());
        if let Decision::Remove { was } = &got {
            removes += 1;
            let l = l.as_ref().expect("Remove requires a lockfile entry");
            assert_eq!(l.state, FileState::Managed, "never remove an adopted file");
            assert_eq!(l.managed, Managed::Full, "never remove a seeded file");
            assert_eq!(
                k.as_ref().and_then(|e| e.digest.clone()),
                Some(was.clone()),
                "only remove bytes we recognise as ours"
            );
        }
    }
    // Non-vacuity: if the corpus stopped producing removals this test would pass while
    // asserting nothing.
    assert!(removes > 0, "corpus must exercise the removal path");
}

#[test]
fn guarantee_never_silently_overwrites_an_operator_edit() {
    let mut edits_seen = 0;
    for (l, k, w) in every_combination() {
        let got = decide(l.as_ref(), k.as_ref(), w.as_ref());
        let Some(disk_digest) = k.as_ref().and_then(|e| e.digest.clone()) else {
            continue;
        };
        // "Edited" means: tracked, and the bytes are neither ours nor an edit we already know.
        let is_edit = l
            .as_ref()
            .is_some_and(|l| !l.is_ours(&disk_digest) && !l.is_known_edit(&disk_digest));
        if is_edit {
            edits_seen += 1;
            assert!(
                !matches!(got, Decision::Replace { .. } | Decision::Add { .. }),
                "edited file got {got:?}, which writes without acknowledging the edit"
            );
        }
    }
    assert!(edits_seen > 0, "corpus must exercise the edited-file path");
}

#[test]
fn classification_is_deterministic() {
    for (l, k, w) in every_combination() {
        let a = decide(l.as_ref(), k.as_ref(), w.as_ref());
        let b = decide(l.as_ref(), k.as_ref(), w.as_ref());
        assert_eq!(a, b);
    }
}

#[test]
fn no_combination_panics() {
    // Exercises the `unreachable!` arms: if digest-equality transitivity were ever violated by
    // a refactor, this is what catches it.
    for (l, k, w) in every_combination() {
        let _ = decide(l.as_ref(), k.as_ref(), w.as_ref());
    }
}

// ---------------------------------------------------------------- driver

#[test]
fn reconcile_reports_in_path_order_and_ignores_operator_files() {
    let lock = lockfile_with(vec![
        locked("mods/keep.jar", d(1)),
        locked("mods/drop.jar", d(2)),
        locked("mods/bump.jar", d(3)),
    ]);

    let disk: DiskState = [
        (p("mods/keep.jar"), on_disk(d(1))),
        (p("mods/drop.jar"), on_disk(d(2))),
        (p("mods/bump.jar"), on_disk(d(3))),
        // Dropped in by the operator; must not appear in the result at all.
        (p("mods/theirs.jar"), on_disk(d(99))),
    ]
    .into_iter()
    .collect();

    let desired: DesiredSet = [
        wanted("mods/keep.jar", d(1)),
        wanted("mods/bump.jar", d(4)),
        wanted("mods/new.jar", d(5)),
    ]
    .into_iter()
    .collect();

    let out = reconcile(Some(&lock), &disk, &desired, &ConflictPolicy::default());
    let by_path: BTreeMap<_, _> = out.iter().map(|(p, d)| (p.as_str(), d)).collect();

    assert_eq!(
        by_path["mods/bump.jar"],
        &Decision::Replace {
            from: d(3),
            to: d(4)
        }
    );
    assert_eq!(by_path["mods/drop.jar"], &Decision::Remove { was: d(2) });
    assert!(matches!(by_path["mods/keep.jar"], Decision::Keep { .. }));
    assert!(matches!(
        by_path["mods/new.jar"],
        Decision::Add { restore: false, .. }
    ));
    assert!(
        !by_path.contains_key("mods/theirs.jar"),
        "operator files are not our business"
    );

    let paths: Vec<&str> = out.iter().map(|(p, _)| p.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "output must be deterministic");
}

#[test]
fn a_fresh_directory_with_existing_content_adopts_rather_than_clobbers() {
    // `hopper <pack> ./existing-server` with no lockfile: nothing can be destroyed, because
    // every pre-existing file is untracked.
    let disk: DiskState = [
        (p("mods/same.jar"), on_disk(d(1))),
        (p("mods/different.jar"), on_disk(d(8))),
        (p("mods/theirs.jar"), on_disk(d(9))),
    ]
    .into_iter()
    .collect();

    let desired: DesiredSet = [
        wanted("mods/same.jar", d(1)),
        wanted("mods/different.jar", d(2)),
        wanted("mods/new.jar", d(3)),
    ]
    .into_iter()
    .collect();

    let out = reconcile(None, &disk, &desired, &ConflictPolicy::default());
    for (_, decision) in &out {
        assert!(
            !matches!(decision, Decision::Remove { .. }),
            "a first install must never delete anything"
        );
    }
    let by_path: BTreeMap<_, _> = out.iter().map(|(p, d)| (p.as_str(), d)).collect();
    assert!(matches!(
        by_path["mods/same.jar"],
        Decision::Adopt {
            reconciled: false,
            ..
        }
    ));
    assert!(matches!(
        by_path["mods/different.jar"],
        Decision::Conflict { .. }
    ));
}

#[test]
fn case_collisions_within_a_pack_are_detectable() {
    // On APFS/NTFS these two race for one slot; we must notice before writing either.
    let desired: DesiredSet = [
        wanted("mods/Sodium.jar", d(1)),
        wanted("mods/sodium.jar", d(2)),
    ]
    .into_iter()
    .collect();
    assert_eq!(desired.case_collisions().len(), 1);
}

#[test]
fn disk_state_finds_disabled_markers() {
    let disk: DiskState = [(p("mods/a.jar.disabled"), on_disk(d(1)))]
        .into_iter()
        .collect();
    assert!(disk.has_disabled_marker(&p("mods/a.jar")));
    assert!(!disk.has_disabled_marker(&p("mods/b.jar")));
}
