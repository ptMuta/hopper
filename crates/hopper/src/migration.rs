//! Verified whole-installation backups and adoption. Source cleanup happens only after
//! both destination validation and mandatory service installation have succeeded.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use hopper_core::{
    fs as hfs,
    model::{Digest, FileState, Provenance, RelPath},
};
use serde::{Deserialize, Serialize};

use crate::interface::{App, Provider, Scope};
use crate::managed::{self, Instance};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    digest: Option<Digest>,
    size: u64,
    mode: u32,
    link: Option<PathBuf>,
}
type Manifest = BTreeMap<String, Entry>;

fn available_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } == 0,
        "checking free disk space: {}",
        std::io::Error::last_os_error()
    );
    let info = unsafe { info.assume_init() };
    Ok(info.f_bavail.saturating_mul(info.f_frsize))
}

fn existing_parent(path: &Path) -> Result<&Path> {
    let mut parent = path;
    while !parent.exists() {
        parent = parent.parent().context("no existing storage parent")?;
    }
    Ok(parent)
}

// Directory ownership alone does not grant permission to remove it from its parent.
// Check before migration and again before cleanup, rather than discovering this after
// remove_dir_all has already removed some of the original installation.
fn verify_removal_permissions(root: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    fn writable_directory(path: &Path) -> Result<()> {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        ensure!(
            unsafe {
                libc::faccessat(
                    libc::AT_FDCWD,
                    name.as_ptr(),
                    libc::W_OK | libc::X_OK,
                    libc::AT_EACCESS,
                )
            } == 0,
            "cannot remove installation: directory {} requires write/search permission",
            path.display()
        );
        Ok(())
    }
    let parent = root.parent().context("installation has no parent")?;
    writable_directory(parent)?;
    let parent_meta = fs::metadata(parent)?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        parent_meta.mode() & libc::S_ISVTX == 0
            || uid == 0
            || parent_meta.uid() == uid
            || fs::symlink_metadata(root)?.uid() == uid,
        "cannot remove installation from a sticky parent owned by another user"
    );
    fn visit(path: &Path) -> Result<()> {
        writable_directory(path)?;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                visit(&entry.path())?;
            }
        }
        Ok(())
    }
    visit(root)
}

fn manifest(root: &Path) -> Result<Manifest> {
    fn visit(root: &Path, dir: &Path, out: &mut Manifest) -> Result<()> {
        for item in fs::read_dir(dir)? {
            let path = item?.path();
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF8 migration path")?
                .to_owned();
            RelPath::parse(&relative)?;
            let meta = fs::symlink_metadata(&path)?;
            let link = if meta.file_type().is_symlink() {
                let link = fs::read_link(&path)?;
                ensure!(
                    !link.is_absolute() && path.canonicalize()?.starts_with(root),
                    "external/absolute symlink in existing installation: {relative}"
                );
                Some(link)
            } else {
                None
            };
            ensure!(
                meta.is_file() || meta.is_dir() || link.is_some(),
                "special file in installation: {relative}"
            );
            let digest = if meta.is_file() {
                Some(hfs::hash_file(&path)?.1)
            } else {
                None
            };
            out.insert(
                relative,
                Entry {
                    digest,
                    size: if meta.is_file() { meta.len() } else { 0 },
                    mode: meta.mode() & 0o7777,
                    link,
                },
            );
            if meta.is_dir() {
                visit(root, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

fn verify_archive(path: &Path, expected: &Manifest) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
    let mut actual = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .to_str()
            .context("invalid backup path")?
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        if name.is_empty() || name == "." {
            continue;
        }
        RelPath::parse(&name)?;
        let mode = entry.header().mode()? & 0o7777;
        let kind = entry.header().entry_type();
        // tar's reader expands GNU sparse holes to zero bytes. Hash the logical
        // contents exactly as manifest() hashes the source, not the stored extents.
        let (digest, size, link) = if kind.is_file() || kind.is_gnu_sparse() {
            let mut hasher = hopper_core::MultiHasher::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = entry.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                hasher.update(&buffer[..n]);
            }
            (Some(hasher.finish().1), size, None)
        } else if kind.is_symlink() {
            (
                None,
                0,
                Some(
                    entry
                        .link_name()?
                        .context("missing symlink target")?
                        .into_owned(),
                ),
            )
        } else {
            ensure!(
                kind.is_dir(),
                "unexpected backup entry type {kind:?} at {name}"
            );
            (None, 0, None)
        };
        ensure!(
            actual
                .insert(
                    name,
                    Entry {
                        digest,
                        size,
                        mode,
                        link
                    }
                )
                .is_none(),
            "duplicate backup entry"
        );
    }
    ensure!(
        &actual == expected,
        "backup verification failed; no source changes made"
    );
    // Reading the entire decoder verifies gzip trailer CRC as well as tar contents.
    let mut decoder = GzDecoder::new(File::open(path)?);
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    Ok(())
}

fn backup(root: &Path, path: &Path) -> Result<Manifest> {
    let root = root.canonicalize()?;
    ensure!(
        !path.starts_with(&root),
        "backup must be outside the source installation"
    );
    let parent = path.parent().context("backup needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let destination = parent
        .canonicalize()?
        .join(path.file_name().context("backup needs a filename")?);
    ensure!(
        !destination.starts_with(&root),
        "backup must be outside the source installation"
    );
    let before = manifest(&root)?;
    let required = before
        .values()
        .map(|e| e.size)
        .sum::<u64>()
        .saturating_mul(101)
        / 100
        + (before.len() as u64).saturating_mul(1024)
        + 1024 * 1024;
    ensure!(
        available_bytes(parent)? >= required,
        "insufficient free space for verified backup (need at least {required} bytes)"
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let gzip = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(gzip);
    builder.follow_symlinks(false);
    builder.append_dir_all(".", &root)?;
    let gzip = builder.into_inner()?;
    let file = gzip.finish()?;
    file.sync_all()?;
    ensure!(
        manifest(&root)? == before,
        "source changed during backup; stop the server and retry"
    );
    verify_archive(path, &before)?;
    hfs::write_private_atomic(
        &path.with_extension("gz.manifest.json"),
        &serde_json::to_vec(&before)?,
    )?;
    fs::set_permissions(
        path.with_extension("gz.manifest.json"),
        fs::Permissions::from_mode(0o600),
    )?;
    Ok(before)
}

pub async fn onboard(
    _app: &App,
    record: &mut Instance,
    from: &Path,
    installed: Option<&str>,
    requested_backup: Option<&Path>,
) -> Result<()> {
    let from = from.canonicalize()?;
    ensure!(
        from.parent().is_some()
            && from != Path::new("/")
            && from.join("server.properties").is_file(),
        "--from must identify a complete existing server directory"
    );
    verify_removal_permissions(&from)?;
    let props = fs::read_to_string(from.join("server.properties"))?;
    if let Some(java) = &record.runtime.java {
        ensure!(
            !java.canonicalize()?.starts_with(&from),
            "--java must be outside the installation being moved; use a stable external JVM path"
        );
    }
    let game_port =
        hopper_core::server::properties::get(&props, "server-port").and_then(|p| p.parse().ok());
    let rcon_port =
        hopper_core::server::properties::get(&props, "rcon.port").and_then(|p| p.parse().ok());
    if let Some(world) = hopper_core::server::properties::get(&props, "level-name") {
        RelPath::parse(world).context(
            "world must be within the installation; external worlds cannot be moved safely",
        )?;
    }
    for property in ["server-port", "rcon.port"] {
        if let Some(port) =
            hopper_core::server::properties::get(&props, property).and_then(|p| p.parse().ok())
        {
            ensure!(
                crate::instances::port_free(port),
                "port {port} is listening; stop this server and disable its old supervisor before onboarding"
            );
        }
    }
    managed::onboarding_ports(record, game_port, rcon_port)?;
    let old_lock = crate::read_lockfile(&from)?;
    let baseline = installed.map(str::to_owned).or_else(|| old_lock.as_ref().and_then(|l| l.pack.version_label.clone()))
        .or_else(|| fs::read(from.join("manifest.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| if record.source.provider == Provider::Curseforge { v["version"].as_str().map(str::to_owned) } else { None }))
        .context("installed pack version is not unambiguously known; supply --installed-version (onboarding never upgrades)")?;
    if record.source.provider == Provider::Gtnh {
        record.runtime.java_major = record
            .runtime
            .java_major
            .or_else(|| old_lock.as_ref().map(|l| l.server.java_major));
        ensure!(
            record.runtime.java_major.is_some(),
            "unmanaged GTNH onboarding requires --java-major to preserve the existing Java distribution (8/17/21/25)"
        );
    }
    let path = if let Some(path) = requested_backup {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        }
    } else {
        let base = match record.scope {
            Scope::User => crate::runtime::data_dir()?.join("backups"),
            Scope::System => PathBuf::from("/var/backups/hopper"),
        };
        base.join(format!(
            "{}-{}.tar.gz",
            record.name,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ))
    };
    println!(
        "Onboard baseline {baseline}; target {:?} {:?}.\nBackup: {}\nMove: {} → {}",
        record.source.channel,
        record.source.pack_version,
        path.display(),
        from.display(),
        record.server().display()
    );
    if record.scope == Scope::System {
        managed::prepare_backup_parent(path.parent().context("missing backup parent")?)?;
    }
    let _manifest = backup(&from, &path)?;
    let destination_parent = existing_parent(&record.root)?;
    let needed = _manifest.values().map(|e| e.size).sum::<u64>()
        + (_manifest.len() as u64).saturating_mul(4096);
    ensure!(
        available_bytes(destination_parent)? >= needed,
        "insufficient destination space; original and verified backup retained"
    );
    record.backup = Some(path);
    record.original = Some(from.clone());
    let target = record.source.clone();
    record.installed_version = Some(baseline.clone());
    record.onboarding_target = Some(target.clone());
    if record.source.provider != Provider::Mrpack {
        record.source.pack_version = Some(baseline.clone());
        record.source.channel = None;
    }
    managed::prepare(record)?;
    ensure!(
        managed::invoke(record, "install", &[])? == 0,
        "baseline preparation failed; original and backup retained; run repair"
    );
    if record.source.provider == Provider::Mrpack {
        let info: serde_json::Value = serde_json::from_str(&managed::capture(record, "inspect")?)?;
        ensure!(
            info["version"].as_str() == Some(baseline.as_str()),
            "archive version differs from installed baseline; original and backup retained"
        );
    }
    ensure!(
        managed::invoke(record, "restore", &[])? == 0,
        "adoption failed; original and backup retained; run repair"
    );
    let frozen_file = record.source.file.clone();
    record.source = target;
    if record.scope == Scope::System {
        record.source.file = frozen_file;
    }
    if let Some(file) = &record.source.file {
        let absolute = file.canonicalize()?;
        record.source.file = Some(if let Ok(relative) = absolute.strip_prefix(&from) {
            record.server().join(relative)
        } else {
            absolute
        });
    }
    record.onboarding_target = None;
    managed::save(record)?;
    Ok(())
}

/// Runs as the instance owner, receiving the verified archive over stdin in system scope.
pub fn restore(record: &Instance) -> Result<()> {
    let mut baseline = crate::read_lockfile(&record.server())?.context("baseline lock missing")?;
    if record.source.provider == Provider::Mrpack {
        ensure!(
            baseline.pack.version_label == record.installed_version,
            "archive differs from the recorded onboarding baseline; original retained"
        );
    }
    let input: Box<dyn Read> = if record.scope == Scope::System {
        Box::new(std::io::stdin())
    } else {
        Box::new(File::open(
            record.backup.as_ref().context("missing backup")?,
        )?)
    };
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    archive.set_preserve_permissions(true);
    let mut present = std::collections::BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .to_str()
            .context("invalid backup name")?
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        if name.is_empty() || name == "." {
            continue;
        }
        RelPath::parse(&name)?;
        present.insert(name.clone());
        // Retain the new baseline ledger and regenerated, relocation-safe launcher.
        if name == ".hopper-launch.sh"
            || name == ".hopper/lock.json"
            || name == ".hopper/prepared.json"
        {
            continue;
        }
        ensure!(
            entry.unpack_in(record.server())?,
            "backup entry escaped destination"
        );
    }
    let mut excludes = Vec::new();
    for file in &mut baseline.files {
        let path = file.path.resolve_under(&record.server());
        if file.path.as_str() == "start.sh"
            || (record.source.provider == Provider::Gtnh
                && (file.path.starts_with_dir("libraries")
                    || matches!(
                        file.path.as_str(),
                        "java9args.txt" | "lwjgl3ify-forgePatches.jar"
                    )
                    || (!file.path.as_str().contains('/')
                        && file.path.as_str().starts_with("forge-")
                        && file.path.as_str().ends_with(".jar"))))
            || matches!(
                file.provenance,
                Provenance::Loader { .. }
                    | Provenance::ServerJar { .. }
                    | Provenance::Generated { .. }
            )
        {
            continue;
        }
        if !present.contains(file.path.as_str()) {
            if path.is_file() {
                fs::remove_file(&path)?;
            }
            excludes.push(file.path.to_string());
            file.disabled = true;
            continue;
        }
        if path.is_file() {
            let disk = hfs::hash_file(&path)?.1;
            if disk != file.digest {
                if file.path.starts_with_dir("mods") {
                    file.state = FileState::Adopted;
                } else {
                    file.user_modified = Some(disk);
                }
            }
        }
    }
    baseline.policy.force_exclude.extend(excludes);
    hfs::write_atomic(
        &record.server().join(".hopper/lock.json"),
        baseline.to_json()?.as_bytes(),
        false,
    )?;
    hfs::write_atomic(
        &record.server().join(".hopper/adoption-complete"),
        b"verified\n",
        false,
    )?;
    Ok(())
}

fn verify_destination(record: &Instance, expected: &Manifest) -> Result<()> {
    let actual = manifest(&record.server())?;
    for (path, entry) in expected {
        // These are deliberately regenerated during adoption/bootstrap, not user data.
        if matches!(
            path.as_str(),
            ".hopper-launch.sh"
                | ".hopper/lock.json"
                | ".hopper/prepared.json"
                | "server.properties"
                | "eula.txt"
        ) {
            continue;
        }
        let copied = actual
            .get(path)
            .with_context(|| format!("migration destination missing {path}; source retained"))?;
        ensure!(
            copied.digest == entry.digest && copied.size == entry.size && copied.link == entry.link,
            "migration destination differs at {path}; source retained"
        );
        ensure!(
            copied.mode & 0o777 == entry.mode & 0o777,
            "migration permissions differ at {path}; source retained"
        );
    }
    Ok(())
}

pub fn finish_move(record: &Instance) -> Result<()> {
    let Some(original) = &record.original else {
        return Ok(());
    };
    if !original.exists() {
        return Ok(());
    }
    verify_removal_permissions(original)?;
    ensure!(
        record.server().join(".hopper/adoption-complete").is_file(),
        "adoption is not complete; source retained"
    );
    let backup = record.backup.as_ref().context("missing verified backup")?;
    let expected: Manifest =
        serde_json::from_slice(&fs::read(backup.with_extension("gz.manifest.json"))?)?;
    ensure!(
        original.canonicalize()? == *original
            && !original.starts_with(&record.root)
            && manifest(original)? == expected,
        "source changed or overlaps destination; cleanup refused"
    );
    verify_archive(backup, &expected)?;
    verify_destination(record, &expected)?;
    fs::remove_dir_all(original)
        .context("removing verified original after successful migration")?;
    println!(
        "Moved original installation. Recoverable backup retained at {}.",
        backup.display()
    );
    Ok(())
}

pub fn repair(record: &Instance) -> Result<()> {
    let root = record.server();
    // Reconciliation recovery is hash/journal based; a fresh apply finishes pending work.
    let lock = crate::read_lockfile(&root)?
        .context("no committed lock; retry installation before marking this instance ready")?;
    ensure!(
        root.join(".hopper-launch.sh").is_file(),
        "managed launcher missing; retry installation"
    );
    if record.source.provider == Provider::Gtnh && lock.server.java_major != 8 {
        for path in ["java9args.txt", "lwjgl3ify-forgePatches.jar"] {
            ensure!(
                root.join(path).is_file(),
                "GTNH runtime is incomplete: {path}"
            );
        }
        ensure!(root.join("libraries").is_dir(), "GTNH libraries missing");
    }
    for file in lock.files.iter().filter(|f| {
        !f.disabled
            && matches!(
                f.provenance,
                Provenance::Loader { .. } | Provenance::ServerJar { .. }
            )
    }) {
        ensure!(
            file.path.resolve_under(&root).is_file(),
            "runtime is incomplete: {}",
            file.path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_world_backup_verifies_and_restores_logical_contents() {
        use clap::Parser;
        use hopper_core::model::{
            LOCK_VERSION, LoaderKind, Lockfile, MinecraftVersion,
            lock::{PackRecord, PolicyRecord, ServerRecord},
        };
        use std::io::{Seek, SeekFrom, Write};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("original");
        fs::create_dir_all(source.join("World/region")).unwrap();
        let region = source.join("World/region/r.0.0.mca");
        let mut file = File::create(&region).unwrap();
        // More than four extents exercises GNU's extended sparse headers too.
        for n in 0..8 {
            file.seek(SeekFrom::Start(n * 128 * 1024)).unwrap();
            file.write_all(&[n as u8 + 1; 4096]).unwrap();
        }
        file.set_len(1024 * 1024).unwrap();
        file.sync_all().unwrap();
        let empty = File::create(source.join("World/region/r.1.0.mca")).unwrap();
        empty.set_len(1024 * 1024).unwrap();
        empty.sync_all().unwrap();

        let archive_path = temp.path().join("backup.tar.gz");
        let expected = backup(&source, &archive_path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(&archive_path).unwrap()));
        assert!(
            archive.entries().unwrap().any(|entry| entry
                .unwrap()
                .header()
                .entry_type()
                .is_gnu_sparse())
        );
        verify_archive(&archive_path, &expected).unwrap();
        let mut corrupt = expected.clone();
        corrupt.get_mut("World/region/r.0.0.mca").unwrap().size += 1;
        assert!(verify_archive(&archive_path, &corrupt).is_err());
        let mut corrupt = expected.clone();
        corrupt.get_mut("World/region/r.0.0.mca").unwrap().digest =
            expected["World/region/r.1.0.mca"].digest.clone();
        assert!(verify_archive(&archive_path, &corrupt).is_err());

        let app = crate::interface::App::try_parse_from([
            "hopper",
            "install",
            "sparse-world",
            "--provider",
            "gtnh",
        ])
        .unwrap();
        let Some(crate::interface::Action::Install(create)) = app.command else {
            panic!()
        };
        let mut record = Instance::new(
            Scope::User,
            "sparse-world",
            create.source.source(create.source.provider.unwrap()),
            create.runtime,
        )
        .unwrap();
        record.root = temp.path().join("managed");
        record.backup = Some(archive_path);
        fs::create_dir_all(record.server().join(".hopper")).unwrap();
        let lock = Lockfile {
            lock_version: LOCK_VERSION,
            generator: "hopper test".into(),
            updated_at: "2026-10-03T00:00:00Z".into(),
            server: ServerRecord {
                minecraft: MinecraftVersion::new("1.7.10"),
                loader: LoaderKind::Forge,
                loader_version: "10.13.4.1614".into(),
                java_major: 25,
                start_script: None,
            },
            pack: PackRecord::default(),
            policy: PolicyRecord::default(),
            files: vec![],
            skipped: vec![],
            unknown: BTreeMap::new(),
        };
        fs::write(
            record.server().join(".hopper/lock.json"),
            lock.to_json().unwrap(),
        )
        .unwrap();
        restore(&record).unwrap();
        verify_destination(&record, &expected).unwrap();
        assert_eq!(
            hfs::hash_file(&region).unwrap(),
            hfs::hash_file(&record.server().join("World/region/r.0.0.mca")).unwrap()
        );
        assert_eq!(
            fs::read(record.server().join("World/region/r.1.0.mca")).unwrap(),
            vec![0; 1024 * 1024]
        );
        assert!(region.exists(), "verification must not remove the original");
    }

    #[test]
    fn unsupported_backup_entries_still_fail_with_path_and_type() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unsupported.tar.gz");
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Fifo);
        header.set_mode(0o600);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "world.fifo", std::io::empty())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let error = verify_archive(&path, &Manifest::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Fifo") && error.contains("world.fifo"),
            "{error}"
        );
    }

    #[test]
    fn backup_round_trip_includes_edited_configs_and_capital_world() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        fs::create_dir_all(root.join("World")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(root.join("World/level.dat"), b"world").unwrap();
        fs::write(root.join("config/edited.cfg"), b"custom=true").unwrap();
        let archive = temp.path().join("backup.tar.gz");
        let expected = backup(&root, &archive).unwrap();
        verify_archive(&archive, &expected).unwrap();
        assert!(expected.contains_key("World/level.dat"));
        assert!(backup(&root, &archive).is_err());
    }
    #[test]
    fn rejects_external_symlinks_before_backup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("World")).unwrap();
        assert!(backup(&root, &temp.path().join("backup.tar.gz")).is_err());
        assert!(!temp.path().join("backup.tar.gz").exists());
    }
    #[test]
    fn backup_cannot_enter_source_through_parent_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        fs::create_dir(&root).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(backup(&root, &alias.join("backup.tar.gz")).is_err());
        assert!(!root.join("backup.tar.gz").exists());
    }
    #[test]
    fn removal_permissions_accept_an_owned_writable_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        fs::create_dir_all(root.join("World")).unwrap();
        verify_removal_permissions(&root).unwrap();
    }
    #[test]
    fn removal_permissions_reject_missing_parent_access_before_cleanup() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("readonly");
        let root = parent.join("server");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("valuable"), b"world").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let result = verify_removal_permissions(&root);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(root.join("valuable")).unwrap(), b"world");
    }
    #[test]
    fn destination_verification_detects_corrupt_world_before_cleanup() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let app = crate::interface::App::try_parse_from([
            "hopper",
            "install",
            "world",
            "--provider",
            "gtnh",
        ])
        .unwrap();
        let Some(crate::interface::Action::Install(create)) = app.command else {
            panic!()
        };
        let mut record = Instance::new(
            Scope::User,
            "world",
            create.source.source(create.source.provider.unwrap()),
            create.runtime,
        )
        .unwrap();
        record.root = temp.path().join("managed");
        let source = temp.path().join("original");
        fs::create_dir_all(source.join("World")).unwrap();
        fs::write(source.join("World/level.dat"), b"valuable").unwrap();
        let expected = manifest(&source).unwrap();
        fs::create_dir_all(record.server().join("World")).unwrap();
        fs::write(record.server().join("World/level.dat"), b"valuable").unwrap();
        verify_destination(&record, &expected).unwrap();
        fs::write(record.server().join("World/level.dat"), b"corrupt!").unwrap();
        assert!(verify_destination(&record, &expected).is_err());
        assert_eq!(
            fs::read(source.join("World/level.dat")).unwrap(),
            b"valuable"
        );
    }
}
