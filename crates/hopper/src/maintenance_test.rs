//! Thread-local service/worker boundary for deterministic maintenance integration tests.
//! Files, archives, reconciliation, journals and restoration use the real implementation.
use std::{
    cell::RefCell,
    fs,
    path::PathBuf,
    process::Command,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use clap::Parser;

use crate::{
    interface::{Action, App, Scope},
    managed::{self, Instance, State},
    recovery::{self, Phase},
};

thread_local! { static BACKEND: RefCell<Option<Rc<Fake>>> = const { RefCell::new(None) }; }
pub fn get() -> Option<Rc<Fake>> {
    BACKEND.with(|b| b.borrow().clone())
}

#[derive(Default)]
pub struct Behavior {
    running: bool,
    pid: u32,
    elapsed: Duration,
    events: Vec<String>,
    unchanged: bool,
    fail_apply: bool,
    fail_backup: bool,
    fail_restore: bool,
    crash_new: bool,
    timeout_new: bool,
    cancel_at: Option<Duration>,
    cancelled: bool,
    free_bytes: Option<u64>,
    mutate_during_backup: bool,
    swap_count: usize,
    fail_swap_at: Option<usize>,
    restart_new: bool,
    update_config: bool,
}

pub struct Fake {
    pub root: PathBuf,
    clock: Instant,
    state: RefCell<Behavior>,
}

impl Fake {
    fn server(&self) -> PathBuf {
        self.root.join("instances/world/server")
    }
    fn new_pack(&self) -> bool {
        crate::read_lockfile(&self.server())
            .ok()
            .flatten()
            .is_some_and(|l| l.pack.version_label.as_deref() == Some("2.9.0-RC-2"))
    }
    fn event(&self, event: &str) {
        self.state.borrow_mut().events.push(event.into());
    }
    pub fn property(&self, property: &str) -> Result<String> {
        let state = self.state.borrow();
        Ok(match property {
            "ActiveState" => if state.running { "active" } else { "inactive" }.into(),
            "MainPID" => {
                if state.running && !(state.crash_new && self.new_pack()) {
                    state.pid.to_string()
                } else {
                    "0".into()
                }
            }
            "NRestarts" => if state.restart_new && self.new_pack() {
                "1"
            } else {
                "0"
            }
            .into(),
            _ => bail!("unexpected service property: {property}"),
        })
    }
    pub fn control(&self, command: &Command, action: &str) -> Result<()> {
        self.event(action);
        let args: Vec<_> = command.get_args().map(|s| s.to_string_lossy()).collect();
        if args.iter().any(|a| a == "stop") {
            self.state.borrow_mut().running = false;
        }
        if args.iter().any(|a| a == "start")
            || (args.iter().any(|a| a == "enable") && args.iter().any(|a| a == "--now"))
        {
            let mut state = self.state.borrow_mut();
            state.running = true;
            state.pid += 1;
            if self.new_pack() {
                fs::write(
                    self.server().join("custom-world/level.dat"),
                    b"converted world",
                )?;
            }
        }
        Ok(())
    }
    pub fn install_units(&self, record: &Instance) -> Result<()> {
        self.event("install-units");
        assert_eq!(record.update, "off", "revert must pause updates");
        Ok(())
    }
    pub fn now(&self) -> Instant {
        self.clock + self.state.borrow().elapsed
    }
    pub fn available_bytes(&self) -> Option<u64> {
        self.state.borrow().free_bytes
    }
    pub fn after_archive(&self, root: &std::path::Path) -> Result<()> {
        if self.state.borrow().mutate_during_backup {
            fs::write(
                root.join("server/custom-world/level.dat"),
                b"external edit during backup",
            )?;
        }
        Ok(())
    }
    pub fn after_swap(&self) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.swap_count += 1;
        if state.fail_swap_at == Some(state.swap_count) {
            bail!("injected interruption after directory rename");
        }
        Ok(())
    }
    pub fn sleep(&self, duration: Duration) {
        let cancel = {
            let mut state = self.state.borrow_mut();
            state.elapsed += duration;
            state.cancel_at.is_some_and(|at| state.elapsed >= at) && !state.cancelled
        };
        if cancel {
            let record = managed::load(Scope::User, "world").unwrap();
            hopper_core::fs::write_private_atomic(
                &managed::cancellation(&record).unwrap(),
                b"operator stop",
            )
            .unwrap();
            self.state.borrow_mut().cancelled = true;
            self.state.borrow_mut().running = false;
        }
    }
    pub fn invoke(&self, record: &Instance, operation: &str, args: &[String]) -> Result<i32> {
        self.event(operation);
        match operation {
            "check" => Ok(if self.state.borrow().unchanged { 0 } else { 10 }),
            "stage" => {
                let previous = crate::read_lockfile(&record.server())?.unwrap();
                let mut template = previous.clone();
                template.pack.version_label = Some("2.9.0-RC-2".into());
                let mut entries = std::collections::BTreeMap::new();
                if self.state.borrow().update_config {
                    let blob = hopper_core::cache::BlobStore::new(record.cache()?)
                        .insert_bytes(b"value=incoming\n", None)?;
                    for mut file in previous.files.clone() {
                        file.digest = blob.digest.clone();
                        file.size = blob.size;
                        file.user_modified = None;
                        entries.insert(file.path.clone(), file);
                    }
                }
                let prepared = crate::PreparedInstall {
                    template,
                    entries,
                    start: None,
                    managed: true,
                    gtnh: true,
                    previous: Some(previous.to_json()?),
                    committed: None,
                    choices: Vec::new(),
                    clean_room: false,
                };
                hopper_core::fs::write_private_atomic(
                    &record.server().join(".hopper/prepared.json"),
                    &serde_json::to_vec(&prepared)?,
                )?;
                Ok(10)
            }
            "apply" => {
                crate::apply_prepared(&record.server(), &record.cache()?)?;
                if self.state.borrow().fail_apply {
                    fs::write(
                        record.server().join("custom-world/level.dat"),
                        b"failed apply",
                    )?;
                    return Ok(1);
                }
                Ok(0)
            }
            "rcon" => Ok(0),
            "start-check" => {
                crate::service::ensure_conflicts_resolved(&record.server())?;
                Ok(0)
            }
            "snapshot-restore" if self.state.borrow().fail_restore => Ok(1),
            _ => recovery::worker(record, operation, args, &record.cache()?),
        }
    }
    pub fn capture(&self, record: &Instance, operation: &str, args: &[String]) -> Result<String> {
        self.event(operation);
        match operation {
            "stage-audit" => Ok(serde_json::to_string(&crate::clean_update::audit(
                &record.server(),
            )?)?),
            "stage-conflicts" => Ok(serde_json::to_string(&crate::review::conflicts(
                &record.server(),
                &record.cache()?,
            )?)?),
            "snapshot-create" => {
                if self.state.borrow().fail_backup {
                    bail!("injected backup failure");
                }
                Ok(serde_json::to_string(&recovery::create(record, &args[0])?)?)
            }
            "inspect" => {
                let lock = crate::read_lockfile(&record.server())?.unwrap();
                Ok(serde_json::json!({"version":lock.pack.version_label,"java_major":lock.server.java_major,"game_port":25565,"rcon_port":25575,"listen_address":""}).to_string())
            }
            "health" => Ok((!(self.state.borrow().timeout_new && self.new_pack())).to_string()),
            _ => bail!("unexpected capture: {operation}"),
        }
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    fake: Rc<Fake>,
    record: Instance,
}

type UpdateFile<'a> = (
    &'a str,
    &'a [u8],
    Option<&'a [u8]>,
    Option<&'a [u8]>,
    hopper_core::model::FileState,
);

impl Fixture {
    fn clean_target(&self, files: &[UpdateFile<'_>]) {
        use hopper_core::{
            cache::BlobStore,
            model::{LockedFile, Managed, OverrideLayer, Provenance, RelPath},
        };
        let store = BlobStore::new(self.record.cache().unwrap());
        let mut old = crate::read_lockfile(&self.record.server())
            .unwrap()
            .unwrap();
        old.files.clear();
        let mut entries = std::collections::BTreeMap::new();
        for (name, base, local, incoming, state) in files {
            let blob = store.insert_bytes(base, None).unwrap();
            let path = RelPath::parse(name).unwrap();
            let file = LockedFile {
                path: path.clone(),
                digest: blob.digest,
                size: blob.size,
                state: *state,
                provenance: Provenance::Override {
                    layer: OverrideLayer::Server,
                },
                managed: Managed::Full,
                executable: false,
                user_modified: None,
                disabled: false,
                mtime_ns: None,
            };
            old.files.push(file.clone());
            if let Some(local) = local {
                let target = path.resolve_under(&self.record.server());
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(target, local).unwrap();
            }
            if let Some(incoming) = incoming {
                let blob = store.insert_bytes(incoming, None).unwrap();
                let mut wanted = file;
                wanted.digest = blob.digest;
                wanted.size = blob.size;
                entries.insert(path, wanted);
            }
        }
        old.server.start_script = Some(store.insert_bytes(b"old launcher", None).unwrap().digest);
        hopper_core::fs::write_private_atomic(
            &self.record.server().join(".hopper/lock.json"),
            old.to_json().unwrap().as_bytes(),
        )
        .unwrap();
        let mut template = old.clone();
        template.pack.version_label = Some("2.9.0-RC-2".into());
        let launcher = store.insert_bytes(b"new launcher", None).unwrap();
        let prepared = crate::PreparedInstall {
            template,
            entries,
            start: Some(crate::StartScript::Write {
                content: "new launcher".into(),
                digest: launcher.digest,
            }),
            managed: true,
            gtnh: true,
            previous: Some(old.to_json().unwrap()),
            committed: None,
            choices: Vec::new(),
            clean_room: true,
        };
        hopper_core::fs::write_private_atomic(
            &self.record.server().join(".hopper/prepared.json"),
            &serde_json::to_vec(&prepared).unwrap(),
        )
        .unwrap();
        crate::clean_update::prepare(&self.record.server(), &store).unwrap();
    }
    fn new(running: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fake = Rc::new(Fake {
            root: temp.path().to_owned(),
            clock: Instant::now(),
            state: RefCell::new(Behavior {
                running,
                pid: 41,
                ..Default::default()
            }),
        });
        BACKEND.with(|b| *b.borrow_mut() = Some(fake.clone()));
        let app =
            App::try_parse_from(["hopper", "install", "world", "--provider", "gtnh"]).unwrap();
        let Some(Action::Install(create)) = app.command else {
            panic!()
        };
        let mut record = Instance::new(
            Scope::User,
            "world",
            create.source.source(crate::interface::Provider::Gtnh),
            create.runtime,
        )
        .unwrap();
        record.state = State::Ready;
        record.storage_ready = true;
        record.started = running;
        record.warn = 0;
        record.update = "daily".into();
        record.restart = "weekly".into();
        record.installed_version = Some("2.8.4".into());
        record.runtime.java_major = Some(21);
        fs::create_dir_all(record.server().join(".hopper")).unwrap();
        fs::create_dir_all(record.server().join("custom-world/DIM-1/region")).unwrap();
        fs::create_dir_all(record.root.join("toolchains/java21/bin")).unwrap();
        fs::write(
            record.root.join("toolchains/java21/bin/java"),
            b"old runtime",
        )
        .unwrap();
        fs::write(
            record.server().join("custom-world/level.dat"),
            b"original world",
        )
        .unwrap();
        fs::write(
            record.server().join("custom-world/DIM-1/region/r.0.0.mca"),
            b"dimension",
        )
        .unwrap();
        fs::write(
            record.server().join("server.properties"),
            b"level-name=custom-world\n",
        )
        .unwrap();
        fs::write(record.server().join(".hopper-launch.sh"), b"old launcher").unwrap();
        let pack = hopper_core::source::mrpack::Mrpack {
            index: hopper_core::source::mrpack::MrpackIndex {
                name: "GTNH".into(),
                version_id: "2.8.4".into(),
                summary: None,
                minecraft: hopper_core::model::MinecraftVersion::new("1.7.10"),
                loader: hopper_core::model::LoaderKind::Forge,
                loader_version: Some("10.13.4.1614".into()),
                files: Vec::new(),
            },
            overrides: Vec::new(),
        };
        let mut lock = crate::lockfile_template(&pack, "gtnh", None);
        lock.server.java_major = 21;
        hopper_core::fs::write_private_atomic(
            &record.server().join(".hopper/lock.json"),
            lock.to_json().unwrap().as_bytes(),
        )
        .unwrap();
        managed::save(&record).unwrap();
        Self {
            _temp: temp,
            fake,
            record,
        }
    }
    fn load(&self) -> Instance {
        managed::load(Scope::User, "world").unwrap()
    }
    fn world(&self) -> Vec<u8> {
        fs::read(self.record.server().join("custom-world/level.dat")).unwrap()
    }
    fn event_index(&self, name: &str) -> usize {
        self.fake
            .state
            .borrow()
            .events
            .iter()
            .position(|e| e == name)
            .unwrap()
    }

    fn stage_config(&self) {
        use hopper_core::{
            cache::BlobStore,
            model::{FileState, LockedFile, Managed, OverrideLayer, Provenance, RelPath},
        };
        let store = BlobStore::new(self.record.cache().unwrap());
        let old = store.insert_bytes(b"value=old\n", None).unwrap();
        let new = store.insert_bytes(b"value=incoming\n", None).unwrap();
        let local = store.insert_bytes(b"value=local\n", None).unwrap();
        let path = RelPath::parse("config/test.cfg").unwrap();
        fs::create_dir_all(self.record.server().join("config")).unwrap();
        fs::write(path.resolve_under(&self.record.server()), b"value=local\n").unwrap();
        let file = LockedFile {
            path: path.clone(),
            digest: old.digest,
            size: old.size,
            state: FileState::Managed,
            provenance: Provenance::Override {
                layer: OverrideLayer::Server,
            },
            managed: Managed::Full,
            executable: false,
            user_modified: Some(local.digest),
            disabled: false,
            mtime_ns: None,
        };
        let mut lock = crate::read_lockfile(&self.record.server())
            .unwrap()
            .unwrap();
        lock.files.push(file.clone());
        hopper_core::fs::write_private_atomic(
            &self.record.server().join(".hopper/lock.json"),
            lock.to_json().unwrap().as_bytes(),
        )
        .unwrap();
        self.fake.state.borrow_mut().update_config = true;
        self.fake.invoke(&self.record, "stage", &[]).unwrap();
        let path = self.record.server().join(".hopper/prepared.json");
        let mut prepared: crate::PreparedInstall =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut incoming = file;
        incoming.digest = new.digest;
        incoming.size = new.size;
        incoming.user_modified = None;
        prepared.entries.insert(incoming.path.clone(), incoming);
        hopper_core::fs::write_private_atomic(&path, &serde_json::to_vec(&prepared).unwrap())
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        BACKEND.with(|b| *b.borrow_mut() = None);
    }
}

#[test]
fn successful_running_update_backs_up_before_apply_and_verifies_startup() {
    let f = Fixture::new(true);
    assert_eq!(managed::maintain(&f.record, true, false, None).unwrap(), 0);
    let current = f.load();
    assert_eq!(current.installed_version.as_deref(), Some("2.9.0-RC-2"));
    assert!(current.maintenance.is_none());
    assert_eq!(current.backups.len(), 1);
    assert!(f.event_index("stopping before maintenance") < f.event_index("snapshot-create"));
    assert!(f.event_index("snapshot-create") < f.event_index("apply"));
    assert!(f.fake.state.borrow().elapsed >= Duration::from_secs(30));
    assert_eq!(f.world(), b"converted world");
    assert_eq!(current.update, "daily");
}

#[test]
fn stopped_update_waits_for_first_start_validation() {
    let f = Fixture::new(false);
    managed::maintain(&f.record, true, false, None).unwrap();
    let mut current = f.load();
    assert_eq!(
        current.maintenance.as_ref().unwrap().phase,
        Phase::AwaitingStart
    );
    assert!(!f.fake.state.borrow().running);
    assert!(recovery::start_gate(&current.server(), Scope::User).is_err());
    managed::start(&mut current).unwrap();
    assert!(f.load().maintenance.is_none());
    assert!(f.fake.state.borrow().running);
}

#[test]
fn apply_failure_restores_whole_world_and_pauses_updates() {
    let f = Fixture::new(false);
    f.fake.state.borrow_mut().fail_apply = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    let current = f.load();
    assert_eq!(f.world(), b"original world");
    assert_eq!(current.source.pack_version.as_deref(), Some("2.8.4"));
    assert_eq!(current.update, "off");
    assert_eq!(current.restart, "weekly");
    assert_eq!(current.runtime.java_major, Some(21));
    assert_eq!(
        fs::read(current.root.join("toolchains/java21/bin/java")).unwrap(),
        b"old runtime"
    );
    assert!(current.maintenance.is_none());
    assert!(!f.fake.state.borrow().running);
}

#[test]
fn crash_and_timeout_both_revert_converted_world_and_restart_old_server() {
    for timeout in [false, true] {
        let mut f = Fixture::new(true);
        if timeout {
            f.fake.state.borrow_mut().timeout_new = true;
            f.record.startup_timeout = 40;
        } else {
            f.fake.state.borrow_mut().crash_new = true;
        }
        managed::save(&f.record).unwrap();
        assert!(managed::maintain(&f.record, true, false, None).is_err());
        assert_eq!(f.world(), b"original world");
        assert!(f.fake.state.borrow().running);
        assert_eq!(f.load().installed_version.as_deref(), Some("2.8.4"));
    }
}

#[test]
fn backup_failure_never_applies_and_restarts_unchanged_server() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().fail_backup = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert!(!f.fake.state.borrow().events.iter().any(|e| e == "apply"));
    assert!(f.fake.state.borrow().running);
    assert_eq!(f.world(), b"original world");
    assert!(f.load().maintenance.is_none());
}

#[test]
fn operator_stop_during_validation_prevents_recovery_restart() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().cancel_at = Some(Duration::from_secs(4));
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert!(!f.fake.state.borrow().running);
    assert_eq!(f.world(), b"original world");
    assert!(
        !f.fake
            .state
            .borrow()
            .events
            .iter()
            .any(|e| e == "starting restored server")
    );
}

#[test]
fn unchanged_updates_and_restart_only_jobs_do_not_create_backups() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().unchanged = true;
    managed::maintain(&f.record, true, false, None).unwrap();
    managed::maintain(&f.record, false, true, None).unwrap();
    assert!(f.load().backups.is_empty());
    assert!(!f.record.root.join("backups").exists());
}

#[test]
fn rolling_retention_keeps_two_and_manual_revert_restores_exact_snapshot() {
    let f = Fixture::new(true);
    for _ in 0..4 {
        let record = f.load();
        managed::maintain(&record, true, false, None).unwrap();
    }
    let mut current = f.load();
    assert_eq!(current.backups.len(), 2);
    assert_eq!(
        fs::read_dir(current.root.join("backups")).unwrap().count(),
        2
    );
    let id = current.backups[0].id.clone();
    fs::write(
        current.server().join("custom-world/level.dat"),
        b"later progress",
    )
    .unwrap();
    recovery::revert(&mut current, Some(&id)).unwrap();
    assert_ne!(f.world(), b"later progress");
    assert_eq!(current.update, "off");
    assert_eq!(current.backups.len(), 2);
}

#[test]
fn corrupt_backup_refuses_revert_before_stopping_or_changing_world() {
    let f = Fixture::new(true);
    managed::maintain(&f.record, true, false, None).unwrap();
    let mut current = f.load();
    let archive = current
        .root
        .join("backups")
        .join(&current.backups[0].id)
        .join("instance.tar.gz");
    fs::write(archive, b"corrupt").unwrap();
    let events = f.fake.state.borrow().events.len();
    let world = f.world();
    assert!(recovery::revert(&mut current, None).is_err());
    assert_eq!(f.world(), world);
    assert!(f.fake.state.borrow().running);
    assert!(
        !f.fake.state.borrow().events[events..]
            .iter()
            .any(|e| e == "stopping before revert")
    );
}

#[test]
fn failed_restore_remains_pending_and_repair_resumes_offline() {
    let f = Fixture::new(false);
    f.fake.state.borrow_mut().fail_apply = true;
    f.fake.state.borrow_mut().fail_restore = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    let mut current = f.load();
    assert_eq!(current.state, State::Pending);
    assert_eq!(
        current.maintenance.as_ref().unwrap().phase,
        Phase::Restoring
    );
    assert!(recovery::start_gate(&current.server(), Scope::User).is_err());
    f.fake.state.borrow_mut().fail_restore = false;
    recovery::repair(&mut current).unwrap();
    assert_eq!(current.state, State::Ready);
    assert!(current.maintenance.is_none());
    assert_eq!(f.world(), b"original world");
}

#[test]
fn interruptions_after_each_directory_rename_resume_without_overlaying_new_files() {
    for fail_at in 1..=4 {
        let f = Fixture::new(false);
        f.fake.state.borrow_mut().fail_apply = true;
        f.fake.state.borrow_mut().fail_swap_at = Some(fail_at);
        assert!(managed::maintain(&f.record, true, false, None).is_err());
        let mut current = f.load();
        assert_eq!(
            current.maintenance.as_ref().unwrap().phase,
            Phase::Restoring
        );
        f.fake.state.borrow_mut().fail_swap_at = None;
        recovery::repair(&mut current).unwrap();
        assert_eq!(f.world(), b"original world");
        assert!(current.maintenance.is_none());
        assert!(!current.server().join(".hopper/prepared.json").exists());
        assert_eq!(
            fs::read_dir(current.root.join("backups")).unwrap().count(),
            1
        );
    }
}

#[test]
fn disk_preflight_failure_leaves_running_server_untouched() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().free_bytes = Some(0);
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert!(f.fake.state.borrow().running);
    assert!(
        !f.fake
            .state
            .borrow()
            .events
            .iter()
            .any(|e| e == "stopping before maintenance" || e == "apply")
    );
    assert!(f.load().backups.is_empty());
}

#[test]
fn source_edit_during_backup_aborts_apply_and_keeps_external_edit() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().mutate_during_backup = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert_eq!(f.world(), b"external edit during backup");
    assert!(f.fake.state.borrow().running);
    assert!(!f.fake.state.borrow().events.iter().any(|e| e == "apply"));
    assert!(f.load().backups.is_empty());
    assert_eq!(
        fs::read_dir(f.record.root.join("backups")).unwrap().count(),
        0
    );
}

#[test]
fn a_restart_loop_is_not_a_successful_start() {
    let f = Fixture::new(true);
    f.fake.state.borrow_mut().restart_new = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert_eq!(f.world(), b"original world");
    assert_eq!(f.load().update, "off");
}

#[test]
fn conflict_choices_apply_without_sidecars_and_keep_local_is_acknowledged() {
    for keep in [true, false] {
        let f = Fixture::new(false);
        f.stage_config();
        let mut conflicts =
            crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].diff.contains("-value=local"));
        assert!(conflicts[0].diff.contains("+value=incoming"));
        conflicts[0].choice.keep = keep;
        crate::review::save(
            &f.record.server(),
            &f.record.cache().unwrap(),
            vec![conflicts.remove(0).choice],
        )
        .unwrap();
        crate::review::validate(&f.record.server(), &f.record.cache().unwrap()).unwrap();
        crate::apply_prepared(&f.record.server(), &f.record.cache().unwrap()).unwrap();
        assert_eq!(
            fs::read(f.record.server().join("config/test.cfg")).unwrap(),
            if keep {
                b"value=local\n".as_slice()
            } else {
                b"value=incoming\n".as_slice()
            }
        );
        assert!(!f.record.server().join("config/test.cfg.new").exists());
        if keep {
            let lock = crate::read_lockfile(&f.record.server()).unwrap().unwrap();
            let file = &lock.files[0];
            assert!(file.user_modified.is_some());
            assert_ne!(file.user_modified.as_ref(), Some(&file.digest));
        }
    }
}

#[test]
fn unattended_conflict_review_skips_without_stopping_or_creating_backups() {
    let f = Fixture::new(true);
    f.stage_config();
    assert!(!recovery::review(&f.record).unwrap());
    assert_eq!(managed::maintain(&f.record, true, false, None).unwrap(), 10);
    assert!(f.fake.state.borrow().running);
    assert!(!f.record.root.join("backups").exists());
    assert_eq!(
        fs::read(f.record.server().join("config/test.cfg")).unwrap(),
        b"value=local\n"
    );
}

#[test]
fn launcher_edit_after_review_is_detected_before_apply() {
    let f = Fixture::new(false);
    let store = hopper_core::cache::BlobStore::new(f.record.cache().unwrap());
    let old = store.insert_bytes(b"old launcher", None).unwrap();
    let new = store.insert_bytes(b"new launcher", None).unwrap();
    let mut lock = crate::read_lockfile(&f.record.server()).unwrap().unwrap();
    lock.server.start_script = Some(old.digest);
    hopper_core::fs::write_private_atomic(
        &f.record.server().join(".hopper/lock.json"),
        lock.to_json().unwrap().as_bytes(),
    )
    .unwrap();
    f.fake.invoke(&f.record, "stage", &[]).unwrap();
    let path = f.record.server().join(".hopper/prepared.json");
    let mut prepared: crate::PreparedInstall =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    prepared.start = Some(crate::StartScript::Write {
        content: "new launcher".into(),
        digest: new.digest,
    });
    hopper_core::fs::write_private_atomic(&path, &serde_json::to_vec(&prepared).unwrap()).unwrap();
    crate::review::save(&f.record.server(), &f.record.cache().unwrap(), Vec::new()).unwrap();
    fs::write(
        f.record.server().join(".hopper-launch.sh"),
        b"operator edit after review",
    )
    .unwrap();
    assert!(crate::review::validate(&f.record.server(), &f.record.cache().unwrap()).is_err());
}

#[test]
fn snapshots_restore_read_only_directories_and_preserve_their_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false);
    let dir = f.record.root.join("toolchains/java21/read-only");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("data"), b"runtime data").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    f.fake.state.borrow_mut().fail_apply = true;
    assert!(managed::maintain(&f.record, true, false, None).is_err());
    assert!(f.load().maintenance.is_none());
    assert_eq!(fs::read(dir.join("data")).unwrap(), b"runtime data");
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o555
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn offline_revert_supports_collections_curseforge_and_file_sources() {
    for provider in [
        crate::interface::Provider::Modrinth,
        crate::interface::Provider::Curseforge,
        crate::interface::Provider::Mrpack,
    ] {
        let f = Fixture::new(false);
        let mut record = f.record.clone();
        record.source.provider = provider;
        record.runtime.java_major = None;
        match provider {
            crate::interface::Provider::Modrinth => {
                record.source.collection = Some("collection123".into());
                record.source.mc = Some("1.7.10".into());
                record.source.loader = Some(crate::cli::Loader::Forge);
            }
            crate::interface::Provider::Curseforge => {
                record.source.pack = Some("123".into());
                let mut lock = crate::read_lockfile(&record.server()).unwrap().unwrap();
                lock.pack.file_id = Some("456".into());
                hopper_core::fs::write_private_atomic(
                    &record.server().join(".hopper/lock.json"),
                    lock.to_json().unwrap().as_bytes(),
                )
                .unwrap();
            }
            crate::interface::Provider::Mrpack => {
                let path = record.root.join("source.mrpack");
                fs::write(&path, b"offline source archive").unwrap();
                record.source.file = Some(path);
            }
            _ => unreachable!(),
        }
        record.source.validate().unwrap();
        managed::save(&record).unwrap();
        recovery::begin(&mut record, false, None).unwrap();
        fs::write(
            record.server().join("custom-world/level.dat"),
            b"later world",
        )
        .unwrap();
        let id = record.backups[0].id.clone();
        recovery::rollback(&mut record, &id, false).unwrap();
        assert_eq!(f.world(), b"original world");
        assert_eq!(record.update, "off");
        assert!(record.maintenance.is_none());
        if provider == crate::interface::Provider::Curseforge {
            assert_eq!(record.source.pack_version.as_deref(), Some("456"));
        } else {
            assert!(record.source.pack_version.is_none());
        }
    }
}

#[test]
fn start_gate_checks_custom_instance_roots_without_inherited_environment() {
    let f = Fixture::new(false);
    let mut record = f.record.clone();
    record.root = f.fake.root.join("custom-storage/world");
    managed::save(&record).unwrap();
    assert!(recovery::start_gate(&record.server(), Scope::User).is_ok());
    record.state = State::Pending;
    managed::save(&record).unwrap();
    assert!(recovery::start_gate(&record.server(), Scope::User).is_err());
    assert!(recovery::start_gate(&f.record.server(), Scope::User).is_err());
}

#[test]
fn keeping_an_untracked_collision_does_not_take_deletion_ownership() {
    let f = Fixture::new(false);
    f.stage_config();
    let mut lock = crate::read_lockfile(&f.record.server()).unwrap().unwrap();
    lock.files.clear();
    hopper_core::fs::write_private_atomic(
        &f.record.server().join(".hopper/lock.json"),
        lock.to_json().unwrap().as_bytes(),
    )
    .unwrap();
    let path = f.record.server().join(".hopper/prepared.json");
    let mut prepared: crate::PreparedInstall =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    prepared.previous = Some(lock.to_json().unwrap());
    hopper_core::fs::write_private_atomic(&path, &serde_json::to_vec(&prepared).unwrap()).unwrap();
    let conflicts =
        crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
    crate::review::save(
        &f.record.server(),
        &f.record.cache().unwrap(),
        conflicts.into_iter().map(|c| c.choice).collect(),
    )
    .unwrap();
    crate::apply_prepared(&f.record.server(), &f.record.cache().unwrap()).unwrap();
    let lock = crate::read_lockfile(&f.record.server()).unwrap().unwrap();
    assert_eq!(lock.files[0].state, hopper_core::model::FileState::Adopted);
}

#[test]
fn edited_file_after_review_invalidates_the_staged_choices() {
    let f = Fixture::new(false);
    f.stage_config();
    let conflicts =
        crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
    crate::review::save(
        &f.record.server(),
        &f.record.cache().unwrap(),
        conflicts.into_iter().map(|c| c.choice).collect(),
    )
    .unwrap();
    fs::write(
        f.record.server().join("config/test.cfg"),
        b"edited after review",
    )
    .unwrap();
    assert!(crate::review::validate(&f.record.server(), &f.record.cache().unwrap()).is_err());
    assert!(crate::apply_prepared(&f.record.server(), &f.record.cache().unwrap()).is_err());
}

#[test]
fn large_adopted_pack_update_prompts_only_for_the_customized_conflict() {
    use hopper_core::model::FileState::*;
    let f = Fixture::new(false);
    let names: Vec<_> = (0..200)
        .map(|n| format!("config/default-{n}.cfg"))
        .collect();
    let mut files: Vec<UpdateFile<'_>> = names
        .iter()
        .map(|name| {
            (
                name.as_str(),
                b"old default\n".as_slice(),
                Some(b"old default\n".as_slice()),
                Some(b"new default\n".as_slice()),
                Adopted,
            )
        })
        .collect();
    files.push((
        "config/custom.cfg",
        b"setting=base\n",
        Some(b"setting=admin\n"),
        Some(b"setting=upstream\n"),
        Managed,
    ));
    f.clean_target(&files);
    let conflicts =
        crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].choice.path.as_str(), "config/custom.cfg");
    assert!(!conflicts[0].reviewed);
    assert!(conflicts[0].diff.contains("-setting=base"));
    assert!(conflicts[0].diff.contains("+setting=admin"));
    assert_eq!(
        fs::read(f.record.server().join("config/default-0.cfg")).unwrap(),
        b"old default\n"
    );
    assert!(!f.record.root.join("update-work/new/custom-world").exists());
}

#[test]
fn clean_room_merges_customizations_and_carries_world_properties_and_custom_mods() {
    use hopper_core::model::FileState::*;
    let f = Fixture::new(false);
    fs::create_dir_all(f.record.server().join("mods")).unwrap();
    fs::write(
        f.record.server().join("mods/admin-extra.jar"),
        b"custom mod",
    )
    .unwrap();
    let props = fs::read(f.record.server().join("server.properties")).unwrap();
    f.clean_target(&[
        (
            "config/edited.cfg",
            b"a=1\nb=1\nc=1\n",
            Some(b"a=2\nb=1\nc=1\n"),
            Some(b"a=1\nb=1\nc=3\n"),
            Managed,
        ),
        (
            "mods/obsolete.jar",
            b"old mod",
            Some(b"old mod"),
            None,
            Adopted,
        ),
        (
            "config/deleted.cfg",
            b"disabled config",
            None,
            Some(b"new defaults"),
            Managed,
        ),
        (
            "config/vanilla.cfg",
            b"base\n",
            Some(b"base\n"),
            Some(b"new defaults\n"),
            Adopted,
        ),
    ]);
    assert!(
        recovery::review(&f.record).unwrap(),
        "non-overlapping edits must not need a prompt"
    );
    let mut record = f.load();
    recovery::begin(&mut record, false, None).unwrap();
    let path = record.server().join(".hopper/prepared.json");
    let prepared: crate::PreparedInstall =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    crate::clean_update::apply(&record.server(), &record.cache().unwrap(), &prepared).unwrap();
    assert_eq!(
        fs::read(record.server().join("config/edited.cfg")).unwrap(),
        b"a=2\nb=1\nc=3\n"
    );
    assert_eq!(
        fs::read(record.server().join("config/vanilla.cfg")).unwrap(),
        b"new defaults\n"
    );
    assert!(!record.server().join("mods/obsolete.jar").exists());
    assert!(!record.server().join("config/deleted.cfg").exists());
    assert_eq!(
        fs::read(record.server().join("mods/admin-extra.jar")).unwrap(),
        b"custom mod"
    );
    assert_eq!(
        fs::read(record.server().join("server.properties")).unwrap(),
        props
    );
    assert_eq!(f.world(), b"original world");
    assert_eq!(
        fs::read(record.server().join(".hopper-launch.sh")).unwrap(),
        b"new launcher"
    );
    let lock = crate::read_lockfile(&record.server()).unwrap().unwrap();
    let edited = lock
        .files
        .iter()
        .find(|f| f.path.as_str() == "config/edited.cfg")
        .unwrap();
    assert!(edited.user_modified.is_some());
    assert_ne!(edited.user_modified.as_ref(), Some(&edited.digest));
    assert!(!record.root.join("update-work/previous").exists());
    let id = record.backups[0].id.clone();
    recovery::rollback(&mut record, &id, false).unwrap();
    assert_eq!(
        fs::read(record.server().join("config/edited.cfg")).unwrap(),
        b"a=2\nb=1\nc=1\n"
    );
    assert_eq!(f.world(), b"original world");
}

#[test]
fn interrupted_clean_room_swap_recovers_from_backup_without_a_live_server_directory() {
    let f = Fixture::new(false);
    f.clean_target(&[]);
    assert!(recovery::review(&f.record).unwrap());
    let mut record = f.load();
    recovery::begin(&mut record, false, None).unwrap();
    fs::rename(record.server(), record.root.join("update-work/previous")).unwrap();
    assert!(!record.server().exists());
    recovery::repair(&mut record).unwrap();
    assert_eq!(f.world(), b"original world");
    assert!(record.maintenance.is_none());
    assert!(!record.root.join("update-work").exists());
}

#[test]
fn clean_room_tampering_is_rejected_before_touching_the_current_installation() {
    use hopper_core::model::FileState::Managed;
    let f = Fixture::new(false);
    f.clean_target(&[("config/a.cfg", b"old", Some(b"old"), Some(b"new"), Managed)]);
    assert!(recovery::review(&f.record).unwrap());
    let path = f.record.server().join(".hopper/prepared.json");
    let prepared: crate::PreparedInstall =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    fs::write(
        f.record.root.join("update-work/new/config/a.cfg"),
        b"tampered",
    )
    .unwrap();
    assert!(
        crate::clean_update::apply(&f.record.server(), &f.record.cache().unwrap(), &prepared)
            .is_err()
    );
    assert_eq!(
        fs::read(f.record.server().join("config/a.cfg")).unwrap(),
        b"old"
    );
    assert_eq!(f.world(), b"original world");
}

#[test]
fn removed_custom_file_requires_an_explicit_keep_or_remove_choice() {
    for keep in [true, false] {
        let f = Fixture::new(false);
        f.clean_target(&[(
            "mods/patched.jar",
            b"baseline mod",
            Some(b"admin patched mod"),
            None,
            hopper_core::model::FileState::Managed,
        )]);
        let mut conflicts =
            crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].removed);
        conflicts[0].choice.keep = keep;
        crate::review::save(
            &f.record.server(),
            &f.record.cache().unwrap(),
            vec![conflicts.remove(0).choice],
        )
        .unwrap();
        let prepared: crate::PreparedInstall = serde_json::from_slice(
            &fs::read(f.record.server().join(".hopper/prepared.json")).unwrap(),
        )
        .unwrap();
        crate::clean_update::apply(&f.record.server(), &f.record.cache().unwrap(), &prepared)
            .unwrap();
        assert_eq!(f.record.server().join("mods/patched.jar").exists(), keep);
    }
}

#[test]
fn successive_merges_use_the_new_upstream_baseline_instead_of_previous_admin_edits() {
    use hopper_core::cache::BlobStore;
    let f = Fixture::new(false);
    f.clean_target(&[(
        "config/edit.cfg",
        b"a=1\nb=1\nc=1\n",
        Some(b"a=2\nb=1\nc=1\n"),
        Some(b"a=1\nb=1\nc=3\n"),
        hopper_core::model::FileState::Managed,
    )]);
    assert!(recovery::review(&f.record).unwrap());
    let path = f.record.server().join(".hopper/prepared.json");
    let prepared: crate::PreparedInstall =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    crate::clean_update::apply(&f.record.server(), &f.record.cache().unwrap(), &prepared).unwrap();
    let old = crate::read_lockfile(&f.record.server()).unwrap().unwrap();
    let store = BlobStore::new(f.record.cache().unwrap());
    assert_eq!(
        fs::read(
            store
                .get_verified(&old.files[0].digest)
                .unwrap()
                .unwrap()
                .path
        )
        .unwrap(),
        b"a=1\nb=1\nc=3\n"
    );
    let mut next = prepared;
    next.previous = Some(old.to_json().unwrap());
    next.template = old.clone();
    next.template.pack.version_label = Some("2.9.0-RC-3".into());
    next.choices.clear();
    let blob = store.insert_bytes(b"a=1\nb=4\nc=3\n", None).unwrap();
    let file = next.entries.values_mut().next().unwrap();
    file.digest = blob.digest;
    file.size = blob.size;
    hopper_core::fs::write_private_atomic(&path, &serde_json::to_vec(&next).unwrap()).unwrap();
    crate::clean_update::prepare(&f.record.server(), &store).unwrap();
    let conflicts =
        crate::review::conflicts(&f.record.server(), &f.record.cache().unwrap()).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].reviewed);
    let merged = conflicts[0].choice.merged.as_ref().unwrap();
    assert_eq!(
        fs::read(store.get_verified(merged).unwrap().unwrap().path).unwrap(),
        b"a=2\nb=4\nc=3\n"
    );
}
