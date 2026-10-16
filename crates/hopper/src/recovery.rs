//! Verified rolling snapshots and durable update/revert orchestration.
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use hopper_core::{fs as hfs, model::Digest};
use serde::{Deserialize, Serialize};

use crate::{
    interface::{Provider, RuntimeOptions, Source},
    managed::{self, Instance, State},
    migration, prompt,
};

const EXCLUDED: &[&str] = &["backups", "update-work", "server/.hopper/prepared.json"];
pub fn default_keep() -> usize {
    2
}
pub fn default_timeout() -> u64 {
    1200
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    source: Source,
    runtime: RuntimeOptions,
    restart: String,
    warn: u32,
    started: bool,
    game_port: Option<u16>,
    rcon_port: Option<u16>,
    listen_address: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub created_at: String,
    pub version: Option<String>,
    pub pin: Option<String>,
    pub java_major: u32,
    pub bytes: u64,
    pub archive_digest: Digest,
    pub manifest_digest: Digest,
    pub verified: bool,
    pub settings: Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    BackingUp,
    Applying,
    AwaitingStart,
    Validating,
    Restoring,
    Restored,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub backup_id: String,
    pub phase: Phase,
    pub was_running: bool,
    pub cancellation: Option<Vec<u8>>,
}

fn settings(record: &Instance) -> Settings {
    Settings {
        source: record.source.clone(),
        runtime: record.runtime.clone(),
        restart: record.restart.clone(),
        warn: record.warn,
        started: record.started,
        game_port: record.game_port,
        rcon_port: record.rcon_port,
        listen_address: record.listen_address.clone(),
    }
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(
        id.starts_with("update-")
            && id.len() > 7
            && id.len() < 64
            && id[7..].bytes().all(|c| c.is_ascii_digit()),
        "invalid backup ID"
    );
    Ok(())
}

fn directory(record: &Instance) -> Result<PathBuf> {
    let path = record.root.join("backups");
    if !path.exists() {
        fs::create_dir(&path)?;
    }
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_dir(),
        "backup store must be a real directory"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn location(record: &Instance, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    Ok(directory(record)?.join(id))
}

fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn move_durable(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    sync_dir(from.parent().context("missing rename source parent")?)?;
    sync_dir(to.parent().context("missing rename destination parent")?)?;
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        fake.after_swap()?;
    }
    Ok(())
}

/// Only used for private backup/work directories; never follows symlinks.
pub(crate) fn remove_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_dir() {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
        )?;
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn required(record: &Instance) -> Result<u64> {
    let manifest = migration::manifest_filtered(&record.root, EXCLUDED)?;
    let bytes = manifest.values().try_fold(0u64, |sum, e| {
        sum.checked_add(e.size).context("backup size overflow")
    })?;
    Ok(bytes
        .saturating_add(bytes / 100)
        .saturating_add((manifest.len() as u64).saturating_mul(1024))
        .saturating_add(1024 * 1024))
}

fn preflight(record: &Instance) -> Result<()> {
    let staged = hfs::read_optional(&record.server().join(".hopper/prepared.json"))?
        .map(|bytes| serde_json::from_slice::<crate::PreparedInstall>(&bytes))
        .transpose()?;
    let growth = staged
        .as_ref()
        .map(|s| s.entries.values().map(|f| f.size).sum::<u64>())
        .unwrap_or(0);
    let copies = if staged.as_ref().is_some_and(|s| s.clean_room) {
        3
    } else {
        2
    };
    let required = required(record)?
        .saturating_mul(copies)
        .saturating_add(growth);
    ensure!(
        migration::available_bytes(&record.root)? >= required,
        "insufficient free space for backup, staged files and restore (need {required} bytes free)"
    );
    Ok(())
}

pub(crate) fn create(record: &Instance, id: &str) -> Result<Snapshot> {
    preflight(record)?;
    let path = location(record, id)?;
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let result = (|| {
        let archive = path.join("instance.tar.gz");
        migration::backup_filtered(&record.root, &archive, EXCLUDED)?;
        let lock =
            crate::read_lockfile(&record.server())?.context("backup requires installed lock")?;
        let snapshot = Snapshot {
            id: id.into(),
            created_at: crate::now_rfc3339(),
            version: lock.pack.version_label.clone(),
            pin: lock
                .pack
                .file_id
                .clone()
                .or_else(|| lock.pack.version_label.clone()),
            java_major: lock.server.java_major,
            bytes: fs::metadata(&archive)?.len(),
            archive_digest: hfs::hash_file(&archive)?.1,
            manifest_digest: hfs::hash_file(&archive.with_extension("gz.manifest.json"))?.1,
            verified: true,
            settings: settings(record),
        };
        sync_dir(&path)?;
        sync_dir(path.parent().context("backup parent missing")?)?;
        Ok(snapshot)
    })();
    if result.is_err() {
        let _ = remove_tree(&path);
    }
    result
}

fn verified_manifest(record: &Instance, snapshot: &Snapshot) -> Result<migration::Manifest> {
    let path = location(record, &snapshot.id)?;
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_dir(),
        "invalid snapshot directory"
    );
    let archive = path.join("instance.tar.gz");
    let manifest = archive.with_extension("gz.manifest.json");
    for file in [&archive, &manifest] {
        ensure!(
            fs::symlink_metadata(file)?.file_type().is_file(),
            "backup must be a regular file"
        );
    }
    ensure!(
        hfs::hash_file(&archive)?.1 == snapshot.archive_digest
            && hfs::hash_file(&manifest)?.1 == snapshot.manifest_digest,
        "backup checksum mismatch; installation untouched"
    );
    let expected = serde_json::from_slice(&fs::read(manifest)?)?;
    migration::verify_archive(&archive, &expected)?;
    Ok(expected)
}

#[derive(Serialize, Deserialize)]
struct Swap {
    names: Vec<(String, bool)>,
    next: usize,
}

fn workspace(record: &Instance, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    Ok(directory(record)?.join(format!(".restore-{id}")))
}

/// All archive extraction runs as the instance owner, including system scope.
fn restore_files(record: &Instance, snapshot: &Snapshot) -> Result<()> {
    let expected = verified_manifest(record, snapshot)?;
    let work = workspace(record, &snapshot.id)?;
    let stage = work.join("staged");
    let displaced = work.join("displaced");
    let journal = work.join("swap.json");
    if !journal.exists() {
        // A failed extraction is disposable; live data has not been moved yet.
        if work.exists() {
            remove_tree(&work)?;
        }
        let logical: u64 = expected.values().map(|e| e.size).sum();
        ensure!(
            migration::available_bytes(&record.root)?
                >= logical
                    .saturating_add(logical / 100)
                    .saturating_add(1024 * 1024),
            "insufficient free space to restore; installation untouched"
        );
        fs::create_dir_all(&stage)?;
        fs::create_dir(&displaced)?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(
            location(record, &snapshot.id)?.join("instance.tar.gz"),
        )?));
        archive.set_preserve_permissions(true);
        for name in expected.keys() {
            hopper_core::model::RelPath::parse(name)?;
            ensure!(
                !name.starts_with("backups/") && name != "backups",
                "backup contains its own store"
            );
        }
        // Archive::unpack defers restrictive directory permissions until children exist.
        archive.unpack(&stage)?;
        ensure!(
            migration::manifest(&stage)? == expected,
            "extracted backup differs; installation untouched"
        );
        for (name, entry) in &expected {
            if entry.digest.is_some() {
                fs::File::open(stage.join(name))?.sync_all()?;
            }
        }
        for name in expected.keys().rev() {
            let path = stage.join(name);
            if fs::symlink_metadata(&path)?.file_type().is_dir() {
                sync_dir(&path)?;
            }
        }
        sync_dir(&stage)?;
        let mut names = std::collections::BTreeSet::new();
        for dir in [&record.root, &stage] {
            for entry in fs::read_dir(dir)? {
                let name = entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid instance path"))?;
                if name != "backups" {
                    names.insert(name);
                }
            }
        }
        let names = names
            .into_iter()
            .map(|name| {
                let exists = fs::symlink_metadata(record.root.join(&name)).is_ok();
                (name, exists)
            })
            .collect();
        hfs::write_private_atomic(&journal, &serde_json::to_vec(&Swap { names, next: 0 })?)?;
    }
    let mut swap: Swap = serde_json::from_slice(&fs::read(&journal)?)?;
    ensure!(swap.next <= swap.names.len(), "invalid restore progress");
    while swap.next < swap.names.len() {
        let (name, existed) = &swap.names[swap.next];
        ensure!(
            name != "backups" && !name.contains('/') && name != "." && name != "..",
            "invalid restore entry"
        );
        let live = record.root.join(name);
        let old = displaced.join(name);
        let incoming = stage.join(name);
        if *existed && fs::symlink_metadata(&old).is_err() {
            move_durable(&live, &old)?;
        }
        if fs::symlink_metadata(&incoming).is_ok() {
            move_durable(&incoming, &live)?;
        }
        swap.next += 1;
        hfs::write_private_atomic(&journal, &serde_json::to_vec(&swap)?)?;
    }
    ensure!(
        migration::manifest_filtered(&record.root, EXCLUDED)? == expected,
        "restored installation differs; displaced files retained"
    );
    Ok(())
}

pub fn worker(record: &Instance, operation: &str, args: &[String], cache: &Path) -> Result<i32> {
    let id = || {
        args.first()
            .context("missing backup ID")
            .map(String::as_str)
    };
    let snapshot = || -> Result<&Snapshot> {
        let id = id()?;
        record
            .backups
            .iter()
            .find(|s| s.id == id)
            .context("unknown backup ID")
    };
    match operation {
        "snapshot-preflight" => {
            directory(record)?;
            preflight(record)?;
        }
        "snapshot-create" => println!("{}", serde_json::to_string(&create(record, id()?)?)?),
        "snapshot-verify" => {
            verified_manifest(record, snapshot()?)?;
        }
        "snapshot-restore" => restore_files(record, snapshot()?)?,
        "snapshot-clean" => {
            let path = workspace(record, id()?)?;
            if path.exists() {
                remove_tree(&path)?;
            }
        }
        "snapshot-prune" => {
            for id in args {
                ensure!(
                    record
                        .maintenance
                        .as_ref()
                        .is_none_or(|m| m.backup_id != *id
                            || (m.phase == Phase::BackingUp
                                && !record.backups.iter().any(|s| s.id == *id))),
                    "cannot prune active recovery backup"
                );
                let path = location(record, id)?;
                if path.exists() {
                    remove_tree(&path)?;
                }
            }
        }
        "stage-conflicts" => println!(
            "{}",
            serde_json::to_string(&crate::review::conflicts(&record.server(), cache)?)?
        ),
        "stage-audit" => println!(
            "{}",
            serde_json::to_string(&crate::clean_update::audit(&record.server())?)?
        ),
        "stage-choices" => crate::review::save(
            &record.server(),
            cache,
            if let Some(json) = args.first() {
                serde_json::from_str(json)?
            } else {
                serde_json::from_reader(std::io::stdin().lock())?
            },
        )?,
        "stage-validate" => crate::review::validate(&record.server(), cache)?,
        "health" => {
            let active = crate::network::current(&record.server())?;
            let props = fs::read_to_string(record.server().join("server.properties"))?;
            let host = crate::network::connect_address(Some(&active.address));
            let password = hopper_core::server::properties::get(&props, "rcon.password")
                .context("missing RCON password")?;
            let rcon = hopper_core::server::rcon::Rcon::connect_timeout(
                (host, active.rcon),
                password,
                Duration::from_secs(2),
            )
            .and_then(|mut r| r.command("list"))
            .is_ok();
            let ping = rcon && hopper_core::server::ping::ping(host, active.game).is_ok();
            println!("{}", rcon && ping);
        }
        _ => bail!("unknown recovery operation"),
    }
    Ok(0)
}

pub fn review(record: &Instance) -> Result<bool> {
    let conflicts: Vec<crate::review::Conflict> =
        serde_json::from_str(&managed::capture(record, "stage-conflicts")?)?;
    if !prompt::json_output() {
        let merged = conflicts
            .iter()
            .filter(|c| c.reviewed && c.choice.merged.is_some())
            .count();
        let unresolved = conflicts.iter().filter(|c| !c.reviewed).count();
        println!(
            "Customization review: {merged} automatically merged, {unresolved} need a decision"
        );
    }
    if conflicts.iter().any(|c| !c.reviewed) && (!prompt::enabled() || prompt::assume_defaults()) {
        for c in conflicts.iter().filter(|c| !c.reviewed) {
            eprintln!("Unresolved conflict: {}", c.choice.path);
        }
        eprintln!(
            "Update skipped; run hopper update {} interactively to review the differences.",
            record.name
        );
        return Ok(false);
    }
    let mut choices = Vec::new();
    for mut conflict in conflicts {
        if !conflict.reviewed {
            println!("{}", conflict.diff);
            conflict.choice.keep = prompt::select(
                "File conflict",
                &[
                    prompt::Item::new("Keep local", true),
                    prompt::Item::new(
                        if conflict.removed {
                            "Remove file"
                        } else {
                            "Use incoming"
                        },
                        false,
                    ),
                ],
                Some(0),
            )?;
        }
        choices.push(conflict.choice);
    }
    ensure!(
        managed::invoke_input(record, "stage-choices", &serde_json::to_vec(&choices)?)? == 0,
        "review changed; server untouched"
    );
    Ok(true)
}

pub fn begin(record: &mut Instance, running: bool, token: Option<Vec<u8>>) -> Result<()> {
    let id = format!(
        "update-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    record.maintenance = Some(Transaction {
        backup_id: id.clone(),
        phase: Phase::BackingUp,
        was_running: running,
        cancellation: token,
    });
    managed::save(record)?;
    let mut snapshot: Snapshot = serde_json::from_str(&managed::capture_args(
        record,
        "snapshot-create",
        std::slice::from_ref(&id),
    )?)?;
    ensure!(
        snapshot.id == id && snapshot.verified,
        "worker returned invalid backup"
    );
    snapshot.settings = settings(record);
    record.backups.push(snapshot);
    record
        .maintenance
        .as_mut()
        .context("missing maintenance")?
        .phase = Phase::Applying;
    record.state = State::Pending;
    managed::save(record)?;
    if !prompt::json_output() {
        println!("Verified pre-update backup: {id} (includes all worlds)");
    }
    prune(record)
}

pub fn prune(record: &mut Instance) -> Result<()> {
    let excess = record
        .backups
        .len()
        .saturating_sub(record.backup_keep.max(1));
    let ids: Vec<_> = record
        .backups
        .iter()
        .take(excess)
        .filter(|s| {
            record
                .maintenance
                .as_ref()
                .is_none_or(|m| m.backup_id != s.id)
        })
        .map(|s| s.id.clone())
        .collect();
    if !ids.is_empty() {
        ensure!(
            managed::invoke(record, "snapshot-prune", &ids)? == 0,
            "backup pruning failed"
        );
        record.backups.retain(|s| !ids.contains(&s.id));
        managed::save(record)?;
    }
    Ok(())
}

pub fn cancelled(record: &Instance) -> Result<bool> {
    let token = fs::read(managed::cancellation(record)?).ok();
    Ok(record
        .maintenance
        .as_ref()
        .is_some_and(|m| token != m.cancellation))
}

pub fn wait_ready(record: &Instance) -> Result<()> {
    let pid = managed::property(record, "MainPID")?;
    let restarts = managed::property(record, "NRestarts")?;
    ensure!(pid != "0", "server exited during startup");
    ensure!(restarts == "0", "server restarted during startup");
    let deadline = now()
        .checked_add(Duration::from_secs(record.startup_timeout))
        .context("startup timeout is too large")?;
    let mut stable = None;
    loop {
        ensure!(
            !cancelled(record)?,
            "maintenance cancelled; no automatic restart"
        );
        ensure!(
            managed::property(record, "ActiveState")? == "active"
                && managed::property(record, "MainPID")? == pid
                && managed::property(record, "NRestarts")? == restarts,
            "server crashed or restarted during validation"
        );
        ensure!(
            now() < deadline,
            "server readiness timed out after {} seconds",
            record.startup_timeout
        );
        let ready = managed::capture(record, "health").is_ok_and(|s| s == "true");
        if ready {
            let since = stable.get_or_insert_with(now);
            if now().duration_since(*since) >= Duration::from_secs(30) {
                return Ok(());
            }
        } else {
            stable = None;
        }
        sleep(Duration::from_secs(2));
    }
}

fn now() -> Instant {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.now();
    }
    Instant::now()
}

fn sleep(duration: Duration) {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        fake.sleep(duration);
        return;
    }
    std::thread::sleep(duration);
}

pub fn finish(record: &mut Instance, running: bool) -> Result<()> {
    record.state = State::Ready;
    record
        .maintenance
        .as_mut()
        .context("missing update progress")?
        .phase = if running {
        Phase::Validating
    } else {
        Phase::AwaitingStart
    };
    managed::save(record)?;
    if running {
        ensure!(!cancelled(record)?, "maintenance cancelled");
        ensure!(
            managed::invoke(record, "start-check", &[])? == 0,
            "restart blocked by conflicts"
        );
        managed::checked(
            managed::ctl(record.scope).args(["reset-failed", &record.unit()]),
            "resetting startup failures",
        )?;
        managed::checked(
            managed::ctl(record.scope).args(["start", &record.unit()]),
            "starting updated server",
        )?;
        wait_ready(record)?;
        complete(record, "update succeeded; startup verified")?;
    } else {
        record.last_maintenance = Some("update applied; startup validation pending".into());
        managed::save(record)?;
    }
    Ok(())
}

pub fn complete(record: &mut Instance, outcome: &str) -> Result<()> {
    let transaction = record.maintenance.take();
    record.last_maintenance = Some(outcome.into());
    if let Err(error) = managed::save(record) {
        record.maintenance = transaction;
        return Err(error);
    }
    Ok(())
}

pub fn rollback(record: &mut Instance, id: &str, running: bool) -> Result<()> {
    let result = rollback_inner(record, id, running);
    if result.is_err()
        && record
            .maintenance
            .as_ref()
            .is_some_and(|m| matches!(m.phase, Phase::Restoring | Phase::Restored))
    {
        managed::checked(
            managed::ctl(record.scope).args(["stop", &record.unit()]),
            "stopping incomplete recovery",
        )?;
        record.state = State::Pending;
        managed::save(record)?;
    }
    result
}

fn rollback_inner(record: &mut Instance, id: &str, running: bool) -> Result<()> {
    let snapshot = record
        .backups
        .iter()
        .find(|s| s.id == id)
        .context("backup not found")?
        .clone();
    ensure!(
        managed::invoke(record, "snapshot-verify", &[id.into()])? == 0,
        "backup verification failed; installation untouched"
    );
    let already_restored = record
        .maintenance
        .as_ref()
        .is_some_and(|m| m.phase == Phase::Restored && m.backup_id == id);
    let token = fs::read(managed::cancellation(record)?).ok();
    let mut transaction = record.maintenance.clone().unwrap_or(Transaction {
        backup_id: id.into(),
        phase: Phase::Restoring,
        was_running: running,
        cancellation: token,
    });
    transaction.backup_id = id.into();
    transaction.phase = if already_restored {
        Phase::Restored
    } else {
        Phase::Restoring
    };
    record.maintenance = Some(transaction);
    record.state = State::Pending;
    managed::save(record)?;
    managed::checked(
        managed::ctl(record.scope).args(["stop", &record.unit()]),
        "stopping before revert",
    )?;
    ensure!(
        managed::property(record, "ActiveState")? == "inactive",
        "server did not stop; restore refused"
    );
    if !already_restored {
        ensure!(
            managed::invoke(record, "snapshot-restore", &[id.into()])? == 0,
            "restore failed; displaced installation and backup retained; run repair"
        );
    }
    let saved = snapshot.settings;
    record.source = saved.source;
    if !matches!(record.source.provider, Provider::Mrpack) && record.source.collection.is_none() {
        record.source.pack_version = snapshot.pin;
        record.source.channel = None;
    }
    record.runtime = saved.runtime;
    if record.source.provider == Provider::Gtnh {
        record.runtime.java_major = Some(snapshot.java_major);
    }
    record.restart = saved.restart;
    record.warn = saved.warn;
    record.started = saved.started;
    record.game_port = saved.game_port;
    record.rcon_port = saved.rcon_port;
    record.listen_address = saved.listen_address;
    record.installed_version = snapshot.version;
    record.update = "off".into();
    record.source.validate()?;
    record.state = State::Ready;
    record
        .maintenance
        .as_mut()
        .context("missing revert progress")?
        .phase = Phase::Restored;
    managed::save(record)?;
    managed::install_units_offline(record)?;
    if running && !cancelled(record)? {
        managed::checked(
            managed::ctl(record.scope).args(["reset-failed", &record.unit()]),
            "resetting server failure",
        )?;
        if let Err(error) = (|| {
            ensure!(
                managed::invoke(record, "start-check", &[])? == 0,
                "restored start blocked"
            );
            managed::checked(
                managed::ctl(record.scope).args(["start", &record.unit()]),
                "starting restored server",
            )?;
            wait_ready(record)
        })() {
            managed::checked(
                managed::ctl(record.scope).args(["stop", &record.unit()]),
                "stopping failed recovery",
            )?;
            return Err(
                error.context("restored server failed validation; copies retained; run repair")
            );
        }
    }
    ensure!(
        managed::invoke(record, "snapshot-clean", &[id.into()])? == 0,
        "restore succeeded but displaced-copy cleanup failed; run repair"
    );
    let outcome = format!(
        "reverted to {}; scheduled updates paused",
        record.installed_version.as_deref().unwrap_or("backup")
    );
    prune(record)?;
    complete(record, &outcome)?;
    if !prompt::json_output() {
        println!("{}: restored {id}; scheduled updates paused", record.name);
    }
    Ok(())
}

pub fn revert(record: &mut Instance, requested: Option<&str>) -> Result<i32> {
    let _guard = managed::operation_lock(record)?;
    ensure!(
        record
            .maintenance
            .as_ref()
            .is_none_or(|m| m.phase == Phase::AwaitingStart),
        "interrupted maintenance; run repair first"
    );
    let snapshot = match requested {
        Some(id) => record.backups.iter().find(|s| s.id == id),
        None => record.backups.last(),
    }
    .context("no matching backup available")?;
    let id = snapshot.id.clone();
    ensure!(
        prompt::proceed(
            &format!(
                "Restore {} from {id} ({})? World progress since that snapshot will be lost",
                record.name,
                snapshot.version.as_deref().unwrap_or("unknown version")
            ),
            false
        )?,
        "revert declined"
    );
    let running = managed::property(record, "ActiveState")? == "active";
    record.maintenance = None;
    rollback(record, &id, running)?;
    Ok(0)
}

pub fn repair(record: &mut Instance) -> Result<bool> {
    let Some(transaction) = record.maintenance.clone() else {
        return Ok(false);
    };
    if transaction.phase == Phase::BackingUp
        && !record.backups.iter().any(|s| s.id == transaction.backup_id)
    {
        ensure!(
            managed::invoke(
                record,
                "snapshot-prune",
                std::slice::from_ref(&transaction.backup_id)
            )? == 0,
            "incomplete backup cleanup failed"
        );
        record.maintenance = None;
        record.state = State::Ready;
        managed::save(record)?;
        if transaction.was_running
            && fs::read(managed::cancellation(record)?).ok() == transaction.cancellation
        {
            managed::checked(
                managed::ctl(record.scope).args(["start", &record.unit()]),
                "restarting unchanged server",
            )?;
        }
    } else {
        rollback(record, &transaction.backup_id, transaction.was_running)?;
    }
    Ok(true)
}

pub fn list(record: &Instance, json: bool) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string(&record.backups)?);
    } else {
        for snapshot in record.backups.iter().rev() {
            println!(
                "{}  {}  {}  {} bytes  {}",
                snapshot.id,
                snapshot.created_at,
                snapshot.version.as_deref().unwrap_or("unknown"),
                snapshot.bytes,
                if snapshot.verified {
                    "verified"
                } else {
                    "unverified"
                }
            );
        }
        if record.backups.is_empty() {
            println!("No update backups yet.");
        }
    }
    Ok(0)
}

pub fn report(record: &Instance, result: &Result<i32>) -> Result<()> {
    if prompt::json_output() {
        let current = managed::load(record.scope, &record.name).unwrap_or_else(|_| record.clone());
        println!(
            "{}",
            serde_json::json!({"instance":current.name, "code":result.as_ref().copied().unwrap_or(1), "outcome":current.last_maintenance, "maintenance":current.maintenance, "backups":current.backups, "error":result.as_ref().err().map(|e| format!("{e:#}"))})
        );
    }
    Ok(())
}

pub fn start_gate(dir: &Path, scope: crate::interface::Scope) -> Result<()> {
    let Some(record) = managed::record_for_server(scope, dir)? else {
        return Ok(());
    };
    let name = &record.name;
    ensure!(
        record.state == State::Ready,
        "start blocked by incomplete maintenance; run hopper repair {name}"
    );
    ensure!(
        record
            .maintenance
            .as_ref()
            .is_none_or(|m| matches!(m.phase, Phase::Validating | Phase::Restored)),
        "start requires hopper start {name} for update validation, or hopper repair {name} for recovery"
    );
    Ok(())
}
