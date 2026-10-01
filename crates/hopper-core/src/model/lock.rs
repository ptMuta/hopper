//! The lockfile: what hopper installed, where it came from, and what it deliberately skipped.
//!
//! Format is pretty-printed JSON at `<server>/.hopper/lock.json`, entries sorted by path.
//! JSON rather than TOML because serde's tagged-enum representations round-trip losslessly in
//! it, `#[serde(flatten)]` buys forward compatibility for free, and people script against it
//! with `jq`. Sorting plus one field per line keeps `git diff` useful anyway.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    Digest, Hashes, LoaderKind, MinecraftVersion, ProjectId, RegistryId, RelPath, VersionId,
};

/// Bumped only for semantic changes. Readers refuse anything newer than they understand rather
/// than silently misinterpreting it; older versions are migrated forward.
pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error(
        "this server was set up by a newer hopper (lockfile v{found}, this build reads v{supported}); upgrade hopper"
    )]
    TooNew { found: u32, supported: u32 },
    #[error("lockfile is malformed: {0}")]
    Malformed(String),
    #[error("lockfile lists {path} more than once")]
    DuplicatePath { path: RelPath },
}

/// Who owns a file, and therefore what we are allowed to do to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// hopper created it. We may overwrite it, and we may delete it when the pack drops it.
    Managed,
    /// The user's file, which the pack happened to want byte-identically. We track it so the
    /// diff is accurate, but we must never delete it — it was theirs first.
    ///
    /// This distinction is what makes installing into a directory that already has content safe
    /// by construction, with no separate adoption code path.
    Adopted,
}

/// How strongly we own a file's *content*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Managed {
    /// Normal: the pack decides the content, we keep it in sync.
    #[default]
    Full,
    /// Written once and then never touched again — `eula.txt`, `server.properties`,
    /// `ops.json`. Tracked so uninstall can clean up, but never updated and never deleted.
    ContentSeed,
}

impl Managed {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Where a file came from. Drives the conflict policy, so that "may I overwrite this?" is
/// answered by provenance rather than by guessing from the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Provenance {
    /// A mod jar from a registry.
    Registry {
        registry: RegistryId,
        project: ProjectId,
        version: VersionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
        /// Human version number (`"0.8.13"`), for rendering `0.8.12 → 0.8.13` in the diff.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version_number: Option<String>,
    },
    /// A `.mrpack` index entry whose download URL is not a registry CDN.
    PackFile { url: String },
    /// A file from the pack's `overrides/` or `server-overrides/` tree.
    Override { layer: OverrideLayer },
    /// Produced by installing the mod loader (libraries, argfiles, launch jars).
    Loader { loader: LoaderKind, version: String },
    /// The vanilla server jar.
    ServerJar { minecraft: MinecraftVersion },
    /// Written by hopper itself.
    Generated { kind: GeneratedKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideLayer {
    /// `overrides/` — applied first.
    Global,
    /// `server-overrides/` — applied second, wins on conflict.
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedKind {
    Eula,
    ServerProperties,
    StartScript,
    JvmArgs,
}

/// One tracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedFile {
    pub path: RelPath,
    /// sha512 of the bytes as hopper last knew them.
    pub digest: Digest,
    pub size: u64,
    pub state: FileState,
    pub provenance: Provenance,

    #[serde(default, skip_serializing_if = "Managed::is_full")]
    pub managed: Managed,
    #[serde(default, skip_serializing_if = "is_false")]
    pub executable: bool,

    /// Set once we have seen and accepted a user edit to this file. Its presence means
    /// "preserve quietly" — we report an edit the first time and never nag about it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_modified: Option<Digest>,

    /// The user renamed this to `<path>.disabled`. Keep tracking it, never restore it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,

    /// `(size, mtime)` fast path so a routine update does not re-hash gigabytes. mtime is a
    /// heuristic and the hash is truth, so `--rehash` forces full verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<u64>,
}

impl LockedFile {
    /// True when the bytes on disk are what hopper put there.
    pub fn is_ours(&self, disk: &Digest) -> bool {
        self.digest == *disk
    }

    /// True when the bytes on disk are a user edit we have already acknowledged.
    pub fn is_known_edit(&self, disk: &Digest) -> bool {
        self.user_modified.as_ref() == Some(disk)
    }
}

/// A file the pack lists that hopper deliberately did not install.
///
/// Recorded so an update can reuse the decision without re-downloading and re-inspecting the
/// jar, and so a decision that later flips can be reported rather than happening silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// Where it would have gone.
    pub path: RelPath,
    /// Artifact identity, so the decision survives a path rename.
    pub hashes: Hashes,
    pub provenance: Provenance,
    pub reason: SkipReason,
    pub decided_by: String,
    pub confidence: Confidence,
    /// Rules revision that produced this decision; a bump forces re-evaluation.
    pub rules_revision: u32,
    /// Hash of the full signal set. Unchanged evidence means the decision can be reused as-is.
    pub evidence_digest: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub jar_inspected: bool,
    /// The user put this file here themselves. Guard against ever deleting it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub present_on_disk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkipReason {
    /// Classified as client-side-only.
    ClientOnly,
    /// The user asked for it to be excluded.
    UserExcluded,
    /// Depends on something that was skipped; installing it alone would crash on startup.
    DependencyCascade { requires: RelPath },
    /// Pack marks it optional and the user opted out.
    OptionalOptOut,
    /// Lives at a path a dedicated server never reads (`shaderpacks/`, `options.txt`).
    ClientOnlyPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub lock_version: u32,
    /// e.g. `"hopper 0.1.0"`. Names who wrote this so a bug report is actionable.
    pub generator: String,
    pub updated_at: String,
    pub server: ServerRecord,
    pub pack: PackRecord,
    pub policy: PolicyRecord,
    /// Sorted by path, unique by path. Enforced on read and on write.
    pub files: Vec<LockedFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedFile>,

    /// Fields written by a newer hopper survive a round-trip through an older one.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unknown: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRecord {
    pub minecraft: MinecraftVersion,
    pub loader: LoaderKind,
    pub loader_version: String,
    pub java_major: u32,
    /// What hopper last wrote to `start.sh`, so an operator's edit is noticed and kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_script: Option<Digest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRecord {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_label: Option<String>,
    /// Exactly what the user typed, so a bare `hopper` can replay it.
    pub source_arg: String,
    /// Where the pack came from, when a registry resolved it. Informational: updates replay
    /// `source_arg`, these say what that resolved to last time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub rules_revision: u32,
    pub include_optional: bool,
    /// The operator asked for mods only, so later runs must not suddenly start installing a
    /// loader and a JVM they never wanted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mods_only: bool,
    /// Persisted so overrides need not be retyped on every update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub force_include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub force_exclude: Vec<String>,
    /// The pack published no server pack and the operator agreed to build the server from the
    /// client pack. Remembered so an update does not ask again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub client_pack_fallback: bool,
    /// The operator chose to go without files whose authors block third-party downloads.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_blocked: bool,
}

impl Lockfile {
    /// Parse, migrating older schemas forward and refusing newer ones.
    pub fn load(bytes: &[u8]) -> Result<Self, LockError> {
        let mut v: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| LockError::Malformed(e.to_string()))?;

        let found = v
            .get("lock_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| LockError::Malformed("missing lock_version".into()))?
            as u32;

        if found > LOCK_VERSION {
            return Err(LockError::TooNew {
                found,
                supported: LOCK_VERSION,
            });
        }
        for step in found..LOCK_VERSION {
            migrate(&mut v, step)?;
        }

        let lock: Lockfile =
            serde_json::from_value(v).map_err(|e| LockError::Malformed(e.to_string()))?;
        lock.validate()?;
        Ok(lock)
    }

    pub fn validate(&self) -> Result<(), LockError> {
        let mut prev: Option<&RelPath> = None;
        for f in &self.files {
            if let Some(p) = prev
                && p >= &f.path
            {
                return Err(if p == &f.path {
                    LockError::DuplicatePath {
                        path: f.path.clone(),
                    }
                } else {
                    LockError::Malformed("files are not sorted by path".into())
                });
            }
            prev = Some(&f.path);
        }
        Ok(())
    }

    /// Serialize with entries sorted, so the on-disk form is deterministic and diffs cleanly.
    pub fn to_json(&self) -> Result<String, LockError> {
        let mut out = self.clone();
        out.files.sort_by(|a, b| a.path.cmp(&b.path));
        out.skipped.sort_by(|a, b| a.path.cmp(&b.path));
        out.validate()?;
        serde_json::to_string_pretty(&out).map_err(|e| LockError::Malformed(e.to_string()))
    }

    /// Look up a tracked file.
    ///
    /// Relies on `files` being sorted by path, which [`Lockfile::load`] and
    /// [`Lockfile::to_json`] both enforce. The debug assertion exists because the failure mode
    /// of an unsorted list is silent -- lookups simply miss, and a caller concludes the file is
    /// untracked -- which is far worse than a loud panic in a test build.
    pub fn file(&self, path: &RelPath) -> Option<&LockedFile> {
        debug_assert!(
            self.files.windows(2).all(|w| w[0].path < w[1].path),
            "Lockfile::file requires files sorted by path; build it with Lockfile::sorted"
        );
        self.files
            .binary_search_by(|f| f.path.cmp(path))
            .ok()
            .map(|i| &self.files[i])
    }

    /// Sort and de-duplicate entries, establishing the invariant `file` depends on.
    pub fn sorted(mut files: Vec<LockedFile>) -> Vec<LockedFile> {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup_by(|a, b| a.path == b.path);
        files
    }
}

/// Migrations run on the raw JSON so an old document need not satisfy today's types.
///
/// No released schema precedes v1, so there is nothing to migrate from yet; each future bump
/// adds one arm here.
fn migrate(_v: &mut serde_json::Value, from: u32) -> Result<(), LockError> {
    Err(LockError::Malformed(format!(
        "no migration path from lockfile v{from}"
    )))
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HashAlgo;

    fn digest(seed: u8) -> Digest {
        Digest::new(HashAlgo::Sha512, &format!("{:0128x}", seed)).unwrap()
    }

    fn locked(path: &str, seed: u8) -> LockedFile {
        LockedFile {
            path: RelPath::parse(path).unwrap(),
            digest: digest(seed),
            size: 10,
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

    fn lockfile(files: Vec<LockedFile>) -> Lockfile {
        Lockfile {
            lock_version: LOCK_VERSION,
            generator: "hopper 0.1.0".into(),
            updated_at: "2026-09-15T12:00:00Z".into(),
            server: ServerRecord {
                minecraft: MinecraftVersion::new("26.3"),
                loader: LoaderKind::Fabric,
                loader_version: "0.17.2".into(),
                java_major: 25,
                start_script: None,
            },
            pack: PackRecord {
                name: "Test Pack".into(),
                version_label: Some("1.0.0".into()),
                source_arg: "test-pack".into(),
                ..Default::default()
            },
            policy: PolicyRecord::default(),
            files,
            skipped: vec![],
            unknown: BTreeMap::new(),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let lock = lockfile(vec![locked("config/a.toml", 1), locked("mods/b.jar", 2)]);
        let json = lock.to_json().unwrap();
        assert_eq!(Lockfile::load(json.as_bytes()).unwrap(), lock);
    }

    #[test]
    fn refuses_a_newer_schema_rather_than_guessing() {
        let mut v: serde_json::Value =
            serde_json::from_str(&lockfile(vec![]).to_json().unwrap()).unwrap();
        v["lock_version"] = serde_json::json!(LOCK_VERSION + 7);
        let err = Lockfile::load(v.to_string().as_bytes()).unwrap_err();
        assert!(matches!(err, LockError::TooNew { .. }), "got {err:?}");
        // The message must tell the user what to actually do.
        assert!(err.to_string().contains("upgrade hopper"));
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        // An older hopper must not silently strip state a newer one depends on.
        let mut v: serde_json::Value =
            serde_json::from_str(&lockfile(vec![]).to_json().unwrap()).unwrap();
        v["future_feature"] = serde_json::json!({"enabled": true});
        let lock = Lockfile::load(v.to_string().as_bytes()).unwrap();
        assert!(lock.unknown.contains_key("future_feature"));
        let back: serde_json::Value = serde_json::from_str(&lock.to_json().unwrap()).unwrap();
        assert_eq!(back["future_feature"]["enabled"], serde_json::json!(true));
    }

    #[test]
    fn writing_sorts_entries_so_diffs_stay_clean() {
        let lock = lockfile(vec![locked("mods/z.jar", 1), locked("config/a.toml", 2)]);
        let json = lock.to_json().unwrap();
        let a = json.find("config/a.toml").unwrap();
        let z = json.find("mods/z.jar").unwrap();
        assert!(a < z, "entries should be written in path order");
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let lock = lockfile(vec![locked("mods/a.jar", 1), locked("mods/a.jar", 2)]);
        assert!(matches!(
            lock.validate().unwrap_err(),
            LockError::DuplicatePath { .. }
        ));
    }

    #[test]
    fn unsorted_files_are_rejected_on_read() {
        let lock = lockfile(vec![locked("mods/z.jar", 1), locked("config/a.toml", 2)]);
        // Bypass to_json's sorting to simulate a hand-edited file.
        let json = serde_json::to_string(&lock).unwrap();
        assert!(Lockfile::load(json.as_bytes()).is_err());
    }

    #[test]
    fn lookup_finds_entries_by_path() {
        let lock = lockfile(vec![locked("config/a.toml", 1), locked("mods/b.jar", 2)]);
        assert!(lock.file(&RelPath::parse("mods/b.jar").unwrap()).is_some());
        assert!(lock.file(&RelPath::parse("mods/c.jar").unwrap()).is_none());
    }

    #[test]
    fn defaults_are_omitted_from_the_written_form() {
        let json = lockfile(vec![locked("mods/a.jar", 1)]).to_json().unwrap();
        // Noise suppression: a 400-entry lockfile should not repeat every default.
        assert!(!json.contains("user_modified"));
        assert!(!json.contains("\"disabled\""));
        assert!(!json.contains("\"executable\""));
    }

    #[test]
    fn distinguishes_our_bytes_from_an_acknowledged_edit() {
        let mut f = locked("config/a.toml", 1);
        f.user_modified = Some(digest(9));
        assert!(f.is_ours(&digest(1)));
        assert!(!f.is_ours(&digest(9)));
        assert!(f.is_known_edit(&digest(9)));
        assert!(!f.is_known_edit(&digest(1)));
    }
}
