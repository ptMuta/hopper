//! Digest-bound review of staged files. Only the coordinator asks questions.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use hopper_core::{
    cache::BlobStore,
    fs as hfs,
    model::{Digest, Lockfile, RelPath},
    plan::{Decision, DesiredFile, DesiredSet, reconcile},
};
use serde::{Deserialize, Serialize};

use crate::{PreparedInstall, StartScript};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
    pub path: RelPath,
    pub local: Digest,
    pub incoming: Digest,
    pub keep: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Conflict {
    pub choice: Choice,
    pub diff: String,
    pub reviewed: bool,
}

fn load(root: &Path) -> Result<PreparedInstall> {
    Ok(serde_json::from_slice(&std::fs::read(
        root.join(".hopper/prepared.json"),
    )?)?)
}

fn decisions(root: &Path, prepared: &PreparedInstall) -> Result<Vec<(RelPath, Decision)>> {
    let lock = crate::read_lockfile(root)?;
    let mut desired = DesiredSet::default();
    for file in prepared.entries.values() {
        desired.insert(DesiredFile {
            path: file.path.clone(),
            content: file.digest.clone(),
            size: Some(file.size),
            executable: file.executable,
            provenance: file.provenance.clone(),
            managed: file.managed,
        });
    }
    let mut paths: Vec<_> = desired.paths().cloned().collect();
    if let Some(lock) = &lock {
        paths.extend(lock.files.iter().map(|f| f.path.clone()));
    }
    Ok(reconcile(
        lock.as_ref(),
        &crate::scan(root, &paths),
        &desired,
        &crate::world_policy(root)?,
    ))
}

fn digest(bytes: &[u8]) -> Digest {
    let mut hash = hopper_core::MultiHasher::new();
    hash.update(bytes);
    hash.finish().1
}

/// A valid single-hunk unified diff; bound output and avoid terminal escape injection.
fn diff(path: &RelPath, local: &[u8], incoming: &[u8]) -> String {
    fn text(bytes: &[u8]) -> Option<&str> {
        std::str::from_utf8(bytes)
            .ok()
            .filter(|s| !s.contains('\0'))
    }
    let (Some(old), Some(new)) = (text(local), text(incoming)) else {
        return format!(
            "{path}: binary file; local {} bytes ({}), incoming {} bytes ({})",
            local.len(),
            digest(local),
            incoming.len(),
            digest(incoming)
        );
    };
    let old: Vec<_> = old.lines().collect();
    let new: Vec<_> = new.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let begin = prefix.saturating_sub(3);
    let old_end = (old.len() - suffix + 3).min(old.len());
    let new_end = (new.len() - suffix + 3).min(new.len());
    let mut out = format!(
        "--- local/{path}\n+++ incoming/{path}\n@@ -{},{} +{},{} @@\n",
        begin + 1,
        old_end - begin,
        begin + 1,
        new_end - begin
    );
    let lines = old[begin..prefix]
        .iter()
        .map(|s| (' ', *s))
        .chain(old[prefix..old.len() - suffix].iter().map(|s| ('-', *s)))
        .chain(new[prefix..new.len() - suffix].iter().map(|s| ('+', *s)))
        .chain(new[new.len() - suffix..new_end].iter().map(|s| (' ', *s)));
    for (n, (marker, line)) in lines.enumerate() {
        if n == 300 || out.len() > 32 * 1024 {
            out.push_str("[diff truncated]\n");
            break;
        }
        out.push(marker);
        for c in line.chars() {
            if !c.is_control() || c == '\t' {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

pub fn conflicts(root: &Path, cache: &Path) -> Result<Vec<Conflict>> {
    let prepared = load(root)?;
    let store = BlobStore::new(cache);
    let mut out = Vec::new();
    for (path, decision) in decisions(root, &prepared)? {
        ensure!(
            !matches!(decision, Decision::Reject { .. }),
            "update rejected at {path}: {decision:?}"
        );
        if let Decision::Conflict {
            desired: Some(incoming),
            on_disk: Some(local),
            ..
        } = decision
        {
            let blob = store
                .get_verified(&incoming)?
                .context("incoming file missing from cache")?;
            let local_bytes = std::fs::read(path.resolve_under(root))?;
            ensure!(
                digest(&local_bytes) == local,
                "file changed during review: {path}"
            );
            let incoming_bytes = std::fs::read(blob.path)?;
            out.push(Conflict {
                diff: diff(&path, &local_bytes, &incoming_bytes),
                choice: Choice {
                    path,
                    local,
                    incoming,
                    keep: true,
                },
                reviewed: false,
            });
        }
    }
    if let Some(start) = &prepared.start {
        let content = match start {
            StartScript::Write { content, .. } | StartScript::KeepEdited { content } => content,
        };
        let path = RelPath::parse(".hopper-launch.sh")?;
        if hfs::kind_of(&root.join(path.as_str()))? == hfs::FileKind::Missing {
            return Ok(out);
        }
        ensure!(
            hfs::kind_of(&root.join(path.as_str()))? == hfs::FileKind::File,
            "launcher must be a regular file"
        );
        let local_bytes = std::fs::read(root.join(path.as_str()))?;
        let choice = Choice {
            path: path.clone(),
            local: digest(&local_bytes),
            incoming: digest(content.as_bytes()),
            keep: true,
        };
        let lock = crate::read_lockfile(root)?;
        if choice.local == choice.incoming
            || lock.as_ref().and_then(|l| l.server.start_script.as_ref()) == Some(&choice.local)
        {
            return Ok(out);
        }
        let reviewed = lock
            .as_ref()
            .and_then(|l| l.unknown.get("reviewed_launcher"))
            .and_then(|v| serde_json::from_value::<Choice>(v.clone()).ok())
            .is_some_and(|c| c.local == choice.local && c.incoming == choice.incoming && c.keep);
        out.push(Conflict {
            choice,
            diff: diff(&path, &local_bytes, content.as_bytes()),
            reviewed,
        });
    }
    Ok(out)
}

pub fn save(root: &Path, cache: &Path, choices: Vec<Choice>) -> Result<()> {
    let current = conflicts(root, cache)?;
    ensure!(
        current.len() == choices.len(),
        "conflicts changed during review; recheck"
    );
    for conflict in current {
        ensure!(
            choices
                .iter()
                .filter(|c| c.path == conflict.choice.path
                    && c.local == conflict.choice.local
                    && c.incoming == conflict.choice.incoming)
                .count()
                == 1,
            "file changed during review: {}",
            conflict.choice.path
        );
    }
    let mut prepared = load(root)?;
    prepared.choices = choices;
    hfs::write_private_atomic(
        &root.join(".hopper/prepared.json"),
        &serde_json::to_vec(&prepared)?,
    )?;
    Ok(())
}

pub fn validate(root: &Path, cache: &Path) -> Result<()> {
    let prepared = load(root)?;
    let lock = crate::read_lockfile(root)?
        .map(|l| l.to_json())
        .transpose()?;
    ensure!(lock == prepared.previous, "lockfile changed after staging");
    let current = conflicts(root, cache)?;
    ensure!(
        current.len() == prepared.choices.len(),
        "conflicts changed after review; rerun update"
    );
    for conflict in current {
        ensure!(
            prepared
                .choices
                .iter()
                .any(|c| c.path == conflict.choice.path
                    && c.local == conflict.choice.local
                    && c.incoming == conflict.choice.incoming),
            "file changed after review: {}",
            conflict.choice.path
        );
    }
    Ok(())
}

pub fn resolve(
    root: &Path,
    prepared: &PreparedInstall,
    decisions: &mut [(RelPath, Decision)],
) -> Result<()> {
    for (path, decision) in decisions {
        ensure!(
            !matches!(decision, Decision::Reject { .. }),
            "update rejected at {path}"
        );
        if let Decision::Conflict {
            desired: Some(incoming),
            on_disk: Some(local),
            ..
        } = decision
        {
            let choice = prepared
                .choices
                .iter()
                .find(|c| c.path == *path && c.local == *local && c.incoming == *incoming)
                .with_context(|| format!("unreviewed conflict at {path}; rerun update"))?;
            *decision = if choice.keep {
                Decision::Preserve {
                    lock: incoming.clone(),
                    on_disk: local.clone(),
                    first_seen: false,
                }
            } else {
                Decision::Replace {
                    from: local.clone(),
                    to: incoming.clone(),
                }
            };
        }
    }
    for choice in &prepared.choices {
        ensure!(
            hfs::hash_file(&choice.path.resolve_under(root))?.1 == choice.local,
            "file changed after review: {}",
            choice.path
        );
    }
    Ok(())
}

pub fn record_choices(prepared: &PreparedInstall, next: &mut Lockfile) {
    let previous = prepared
        .previous
        .as_ref()
        .and_then(|s| Lockfile::load(s.as_bytes()).ok());
    for choice in &prepared.choices {
        if choice.path.as_str() == ".hopper-launch.sh" {
            next.server.start_script = Some(choice.incoming.clone());
            next.unknown.insert(
                "reviewed_launcher".into(),
                serde_json::to_value(choice).expect("serializable choice"),
            );
        } else if choice.keep {
            if let Some(entry) = prepared.entries.get(&choice.path) {
                let mut entry = entry.clone();
                entry.state = previous
                    .as_ref()
                    .and_then(|l| l.file(&choice.path))
                    .map_or(hopper_core::model::FileState::Adopted, |f| f.state);
                entry.user_modified = Some(choice.local.clone());
                next.files.retain(|f| f.path != choice.path);
                next.files.push(entry);
            }
        }
    }
    next.files.sort_by(|a, b| a.path.cmp(&b.path));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unified_diff_shows_both_sides_and_sanitizes_control_characters() {
        let diff = diff(
            &RelPath::parse("config/test.cfg").unwrap(),
            b"a\nold\x1b\n",
            b"a\nnew\n",
        );
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
        assert!(!diff.contains('\x1b'));
    }
    #[test]
    fn binary_diff_has_sizes_and_hashes() {
        let diff = diff(&RelPath::parse("mods/test.jar").unwrap(), b"a\0", b"b\0c");
        assert!(diff.contains("local 2 bytes"));
        assert!(diff.contains("incoming 3 bytes"));
    }
}
