//! Build the new pack in isolation, then carry customizations and stopped-server data.
use crate::{PreparedInstall, StartScript};
use anyhow::{Context, Result, ensure};
use hopper_core::{
    cache::BlobStore,
    fs as hfs,
    model::{FileState, Lockfile, Managed, RelPath},
};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub struct Audit {
    version: Option<String>,
    modified: Vec<RelPath>,
    deleted: Vec<RelPath>,
}
impl Audit {
    pub fn report(&self) {
        println!(
            "Customizations against installed {}: {} modified, {} deleted",
            self.version.as_deref().unwrap_or("baseline"),
            self.modified.len(),
            self.deleted.len()
        );
        for path in &self.modified {
            println!("  modified: {path}");
        }
        for path in &self.deleted {
            println!("  deleted: {path}");
        }
    }
}

pub fn audit(root: &Path) -> Result<Audit> {
    let lock = crate::read_lockfile(root)?.context("installed baseline is missing")?;
    let mut audit = Audit {
        version: lock.pack.version_label.clone(),
        modified: Vec::new(),
        deleted: Vec::new(),
    };
    for file in &lock.files {
        if file.managed == Managed::ContentSeed || operator_file(&file.path) {
            continue;
        }
        let path = file.path.resolve_under(root);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if hfs::hash_file(&path)?.1 != file.digest {
                    audit.modified.push(file.path.clone());
                }
            }
            Ok(_) => audit.modified.push(file.path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                audit.deleted.push(file.path.clone())
            }
            Err(e) => return Err(e.into()),
        }
    }
    let launcher = RelPath::parse(".hopper-launch.sh")?;
    if let Some(base) = &lock.server.start_script {
        if hfs::hash_file(&launcher.resolve_under(root)).is_ok_and(|(_, d)| d != *base) {
            audit.modified.push(launcher);
        }
    }
    Ok(audit)
}

fn operator_file(path: &RelPath) -> bool {
    ["server.properties", "eula.txt", "jvm.args"].contains(&path.as_str())
}
fn room(root: &Path) -> Result<PathBuf> {
    Ok(root
        .parent()
        .context("server has no instance directory")?
        .join("update-work/new"))
}

pub fn prepare(root: &Path, store: &BlobStore) -> Result<()> {
    let destination = room(root)?;
    let work = destination.parent().context("missing update workspace")?;
    if work.exists() {
        ensure!(
            fs::symlink_metadata(work)?.file_type().is_dir(),
            "update workspace must be a real directory"
        );
        crate::recovery::remove_tree(work)?;
    }
    fs::create_dir_all(&destination)?;
    fs::set_permissions(work, fs::Permissions::from_mode(0o700))?;
    let path = root.join(".hopper/prepared.json");
    let mut prepared: PreparedInstall = serde_json::from_slice(&fs::read(&path)?)?;
    prepared.entries.retain(|path, _| !operator_file(path));
    let policy = crate::world_policy(root)?;
    for file in prepared.entries.values() {
        ensure!(
            !hopper_core::plan::reconcile::is_protected(&file.path)
                && !policy
                    .protected_dirs
                    .iter()
                    .any(|p| file.path.as_str() == p || file.path.starts_with_dir(p)),
            "new pack targets protected path {}",
            file.path
        );
        ensure!(
            store.get_verified(&file.digest)?.is_some(),
            "staged blob missing for {}",
            file.path
        );
        store.materialize(
            &file.digest,
            &file.path.resolve_under(&destination),
            file.executable,
        )?;
    }
    if let Some(start) = &prepared.start {
        let content = match start {
            StartScript::Write { content, .. } | StartScript::KeepEdited { content } => content,
        };
        hfs::write_atomic(
            &destination.join(".hopper-launch.sh"),
            content.as_bytes(),
            true,
        )?;
        store.insert_bytes(content.as_bytes(), None)?;
    }
    hfs::write_private_atomic(&path, &serde_json::to_vec(&prepared)?)?;
    println!(
        "Clean target {} staged; the current installation is unchanged.",
        prepared
            .template
            .pack
            .version_label
            .as_deref()
            .unwrap_or("version")
    );
    Ok(())
}

fn carry(
    root: &Path,
    destination: &Path,
    dir: &Path,
    old: &Lockfile,
    prepared: &PreparedInstall,
    next: &mut Lockfile,
    modes: &mut Vec<(PathBuf, fs::Permissions)>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let relative = path.strip_prefix(root)?;
        let name = relative.to_str().context("invalid server path")?;
        if [
            ".hopper/lock.json",
            ".hopper/prepared.json",
            ".hopper/journal.json",
        ]
        .contains(&name)
        {
            continue;
        }
        let rel = RelPath::parse(name)?;
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_dir() {
            let target = destination.join(relative);
            fs::create_dir_all(&target)?;
            carry(root, destination, &path, old, prepared, next, modes)?;
            modes.push((target, meta.permissions()));
            continue;
        }
        let choice = prepared.choices.iter().find(|c| c.path == rel);
        let baseline = old.file(&rel);
        let incoming = prepared.entries.get(&rel);
        let local = if meta.is_file() {
            Some(hfs::hash_file(&path)?.1)
        } else {
            None
        };
        let preserve = if operator_file(&rel) {
            true
        } else if name == ".hopper-launch.sh" {
            choice.is_some_and(|c| c.keep) || prepared.start.is_none()
        } else if let Some(choice) = choice {
            choice.keep
        } else if let Some(baseline) = baseline {
            baseline.managed == Managed::ContentSeed || local.as_ref() != Some(&baseline.digest)
        } else {
            incoming.is_none()
        };
        if !preserve {
            continue;
        }
        let target = destination.join(relative);
        fs::create_dir_all(target.parent().context("missing destination parent")?)?;
        ensure!(
            !fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink() || m.is_dir()),
            "unsafe overlay target {name}"
        );
        if meta.file_type().is_symlink() {
            let link = fs::read_link(&path)?;
            ensure!(
                !link.is_absolute() && path.canonicalize()?.starts_with(root),
                "external symlink in server: {name}"
            );
            std::os::unix::fs::symlink(link, &target)?;
        } else {
            ensure!(meta.is_file(), "special file in server: {name}");
            fs::copy(&path, &target)?;
            let copied = hfs::hash_file(&target)?.1;
            ensure!(
                Some(&copied) == local.as_ref() && hfs::hash_file(&path)?.1 == copied,
                "server file changed during copy: {name}"
            );
            fs::File::open(&target)?.sync_all()?;
            if let Some(file) = next.files.iter_mut().find(|f| f.path == rel) {
                if file.digest != copied {
                    file.user_modified = Some(copied);
                }
            }
        }
    }
    Ok(())
}

pub fn apply(root: &Path, cache: &Path, prepared: &PreparedInstall) -> Result<i32> {
    crate::review::validate(root, cache)?;
    let old = crate::read_lockfile(root)?.context("installed baseline missing")?;
    let destination = room(root)?;
    ensure!(
        fs::symlink_metadata(&destination)?.file_type().is_dir(),
        "clean target is missing; rerun update"
    );
    let store = BlobStore::new(cache);
    let contents = crate::migration::manifest(&destination)?;
    for name in contents.keys() {
        let meta = fs::symlink_metadata(destination.join(name))?;
        ensure!(
            meta.file_type().is_dir() || meta.file_type().is_file(),
            "unsafe clean target entry {name}"
        );
    }
    let expected_count = prepared.entries.len() + usize::from(prepared.start.is_some());
    ensure!(
        contents.values().filter(|f| f.digest.is_some()).count() == expected_count,
        "unexpected files in clean target; rerun update"
    );
    // Verify the pristine room before carrying any mutable state into it.
    for file in prepared.entries.values() {
        ensure!(
            hfs::hash_file(&file.path.resolve_under(&destination))?.1 == file.digest,
            "clean target changed: {}",
            file.path
        );
    }
    if let Some(start) = &prepared.start {
        let bytes = match start {
            StartScript::Write { content, .. } | StartScript::KeepEdited { content } => {
                content.as_bytes()
            }
        };
        let mut hash = hopper_core::MultiHasher::new();
        hash.update(bytes);
        ensure!(
            hfs::hash_file(&destination.join(".hopper-launch.sh"))?.1 == hash.finish().1,
            "clean target launcher changed; rerun update"
        );
    }
    let mut next = prepared.template.clone();
    next.files = prepared.entries.values().cloned().collect();
    for file in &mut next.files {
        file.state = FileState::Managed;
    }
    let mut modes = Vec::new();
    carry(
        root,
        &destination,
        root,
        &old,
        prepared,
        &mut next,
        &mut modes,
    )?;
    for choice in &prepared.choices {
        if let Some(merged) = &choice.merged {
            ensure!(
                store.get_verified(merged)?.is_some(),
                "merged blob missing: {}",
                choice.path
            );
            let executable = prepared
                .entries
                .get(&choice.path)
                .is_some_and(|file| file.executable);
            store.materialize(merged, &choice.path.resolve_under(&destination), executable)?;
        }
    }
    for file in &old.files {
        if !file.path.resolve_under(root).exists() {
            if let Some(wanted) = prepared.entries.get(&file.path) {
                let path = wanted.path.resolve_under(&destination);
                if path.exists() {
                    fs::remove_file(path)?;
                }
                if let Some(file) = next.files.iter_mut().find(|f| f.path == wanted.path) {
                    file.disabled = true;
                }
            }
        }
    }
    crate::review::record_choices(prepared, &mut next);
    if let Some(start) = &prepared.start {
        let bytes = match start {
            StartScript::Write { content, .. } | StartScript::KeepEdited { content } => {
                content.as_bytes()
            }
        };
        let mut hash = hopper_core::MultiHasher::new();
        hash.update(bytes);
        next.server.start_script = Some(hash.finish().1);
    }
    hfs::write_private_atomic(
        &destination.join(".hopper/lock.json"),
        next.to_json()?.as_bytes(),
    )?;
    hfs::fsync_dir(&destination)?;
    for (path, mode) in modes {
        fs::set_permissions(&path, mode)?;
        hfs::fsync_dir(&path)?;
    }
    fs::set_permissions(&destination, fs::metadata(root)?.permissions())?;
    hfs::fsync_dir(&destination)?;
    let previous = destination
        .parent()
        .context("missing update workspace")?
        .join("previous");
    ensure!(!previous.exists(), "interrupted update; run repair");
    fs::rename(root, &previous)?;
    hfs::fsync_dir(root.parent().context("missing instance directory")?)?;
    hfs::fsync_dir(previous.parent().context("missing update directory")?)?;
    fs::rename(&destination, root)?;
    hfs::fsync_dir(root.parent().context("missing instance directory")?)?;
    hfs::fsync_dir(previous.parent().context("missing update directory")?)?;
    // The verified rolling backup owns recovery; discard this temporary duplicate.
    crate::recovery::remove_tree(&previous)?;
    Ok(0)
}
