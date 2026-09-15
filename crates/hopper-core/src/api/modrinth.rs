//! The Modrinth API (v2, with v3 used only for collections).
//!
//! v2 is the stable, documented surface. v3 exists in production but Modrinth describe it as
//! experimental and it renames fields (`title` → `name`, `description` → `summary`), so we
//! target v2 everywhere except collections, which have no v2 route.
//!
//! Two things shape this module more than anything else:
//!
//! * **Batching beats parallelism.** `POST /v2/version_files/update` takes a list of hashes and
//!   returns the newest compatible version for each, so checking a 300-mod pack costs one
//!   request rather than 300. Nothing here should ever loop over files issuing one request
//!   each when a batch endpoint exists.
//! * **The User-Agent is mandatory.** Modrinth ask for `owner/project/version (contact)` and
//!   say generic library UAs are more likely to be blocked.

use std::collections::HashMap;

use serde::Deserialize;
use url::Url;

use crate::model::{
    Digest, EnvClaim, EnvSignal, EnvSource, HashAlgo, Hashes, LoaderKind, MinecraftVersion,
    ProjectId, SideSupport, VersionEnvironment, VersionId,
};

pub const API_V2: &str = "https://api.modrinth.com/v2";
pub const API_V3: &str = "https://api.modrinth.com/v3";

/// Modrinth's documented batch ceiling is generous; we chunk well below it so one oversized
/// pack cannot produce a request the server rejects wholesale.
pub const MAX_HASHES_PER_BATCH: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionType {
    Release,
    Beta,
    Alpha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    Required,
    Optional,
    Incompatible,
    Embedded,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireDependency {
    #[serde(default)]
    pub version_id: Option<VersionId>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub file_name: Option<String>,
    pub dependency_type: DependencyType,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireVersionFile {
    #[serde(default)]
    pub hashes: HashMap<String, String>,
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireVersion {
    pub id: VersionId,
    pub project_id: ProjectId,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version_number: String,
    #[serde(default)]
    pub version_type: Option<VersionType>,
    #[serde(default)]
    pub date_published: Option<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub files: Vec<WireVersionFile>,
    #[serde(default)]
    pub dependencies: Vec<WireDependency>,
    /// Per-version environment. Preferred over the project-level fields when present — it is
    /// granular, and a mod can legitimately change sides between versions.
    #[serde(default)]
    pub environment: Option<VersionEnvironment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireProject {
    pub id: ProjectId,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub project_type: String,
    /// Deprecated but still populated. Weak evidence, used only as a fallback.
    #[serde(default)]
    pub client_side: Option<SideSupport>,
    #[serde(default)]
    pub server_side: Option<SideSupport>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireCollection {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Bare project ids. **Collections pin no versions**, so a client must choose one per
    /// project itself — which is why installing a collection needs a target Minecraft version
    /// and loader that a `.mrpack` would have declared.
    #[serde(default)]
    pub projects: Vec<ProjectId>,
}

/// The primary downloadable file of a version, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFile {
    pub filename: String,
    pub url: Url,
    pub hashes: Hashes,
    pub size: u64,
}

/// A version, in our own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub id: VersionId,
    pub project: ProjectId,
    pub name: String,
    pub version_number: String,
    pub version_type: Option<VersionType>,
    pub date_published: Option<String>,
    pub game_versions: Vec<MinecraftVersion>,
    pub loaders: Vec<String>,
    pub file: VersionFile,
    pub dependencies: Vec<WireDependency>,
    pub environment: Option<VersionEnvironment>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    #[error("version {0} has no downloadable file")]
    NoFile(String),
    #[error("version {version} file {filename:?} has an unusable url {url:?}")]
    BadUrl {
        version: String,
        filename: String,
        url: String,
    },
    #[error("version {version} file {filename:?} is missing a sha512 hash")]
    NoSha512 { version: String, filename: String },
}

impl WireVersion {
    /// Convert to the domain type, keeping only the primary file.
    ///
    /// A version can carry several files (sources jars, javadoc). The primary one is what gets
    /// installed; if none is flagged primary we take the first, which is what the API's own
    /// ordering implies.
    pub fn into_version(self) -> Result<Version, ConvertError> {
        let vid = self.id.as_str().to_owned();
        let wire_file = self
            .files
            .iter()
            .find(|f| f.primary)
            .or_else(|| self.files.first())
            .cloned()
            .ok_or_else(|| ConvertError::NoFile(vid.clone()))?;

        let url = Url::parse(&wire_file.url).map_err(|_| ConvertError::BadUrl {
            version: vid.clone(),
            filename: wire_file.filename.clone(),
            url: wire_file.url.clone(),
        })?;

        let mut hashes = Hashes::default();
        for algo in [HashAlgo::Sha1, HashAlgo::Sha512] {
            if let Some(hex) = wire_file.hashes.get(algo.name())
                && let Ok(d) = Digest::new(algo, hex)
            {
                hashes.set(d);
            }
        }
        if hashes.get(HashAlgo::Sha512).is_none() {
            return Err(ConvertError::NoSha512 {
                version: vid,
                filename: wire_file.filename,
            });
        }

        Ok(Version {
            id: self.id,
            project: self.project_id,
            name: self.name,
            version_number: self.version_number,
            version_type: self.version_type,
            date_published: self.date_published,
            game_versions: self
                .game_versions
                .into_iter()
                .map(MinecraftVersion::new)
                .collect(),
            loaders: self.loaders,
            file: VersionFile {
                filename: wire_file.filename,
                url,
                hashes,
                size: wire_file.size,
            },
            dependencies: self.dependencies,
            environment: self.environment,
        })
    }
}

impl Version {
    /// Environment evidence this version contributes to classification.
    pub fn env_signals(&self) -> Vec<EnvSignal> {
        let mut out = Vec::new();
        if let Some(env) = self.environment {
            out.push(EnvSignal::new(
                EnvSource::RegistryVersionEnv,
                env.claim(),
                format!("version environment: {env:?}"),
            ));
        }
        out
    }

    pub fn supports(&self, mc: &MinecraftVersion, loader: LoaderKind) -> bool {
        let loader_ok =
            loader == LoaderKind::Vanilla || self.loaders.iter().any(|l| l == loader.api_name());
        loader_ok && self.game_versions.contains(mc)
    }
}

impl WireProject {
    /// Fallback environment evidence, used when a version carries no `environment`.
    pub fn env_signals(&self) -> Vec<EnvSignal> {
        let Some(server) = self.server_side else {
            return Vec::new();
        };
        let claim = SideSupport::claim_for_server(server);
        if claim == EnvClaim::Neutral {
            return Vec::new();
        }
        vec![EnvSignal::new(
            EnvSource::RegistryProjectSide,
            claim,
            format!("project server_side: {server:?}"),
        )]
    }
}

/// Split a hash list into batches small enough for one request.
pub fn hash_batches(hashes: &[Digest]) -> impl Iterator<Item = &[Digest]> {
    hashes.chunks(MAX_HASHES_PER_BATCH)
}

/// Build the `User-Agent` Modrinth asks third-party clients to send.
pub fn user_agent(repo_owner: &str, version: &str, contact: &str) -> String {
    format!("{repo_owner}/hopper/{version} (+{contact})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA512: &str = "ab00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f";
    const SHA1: &str = "ab0000000000000000000000000000000000000f";

    fn version_json(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "id": "SMxNOGZ6",
            "project_id": "AANobbMI",
            "name": "Sodium 0.8.13 for Fabric 1.21.1",
            "version_number": "mc1.21.1-0.8.13-fabric",
            "version_type": "release",
            "date_published": "2026-08-28T20:55:02.424329Z",
            "game_versions": ["1.21.1"],
            "loaders": ["fabric"],
            "files": [{
                "hashes": { "sha1": SHA1, "sha512": SHA512 },
                "url": "https://cdn.modrinth.com/data/AANobbMI/versions/SMxNOGZ6/sodium.jar",
                "filename": "sodium-fabric-0.8.13.jar",
                "primary": true,
                "size": 1574609
            }],
            "dependencies": []
        });
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn parse(v: serde_json::Value) -> Version {
        serde_json::from_value::<WireVersion>(v)
            .unwrap()
            .into_version()
            .unwrap()
    }

    #[test]
    fn parses_a_real_version_shape() {
        let v = parse(version_json(serde_json::json!({})));
        assert_eq!(v.id.as_str(), "SMxNOGZ6");
        assert_eq!(v.version_number, "mc1.21.1-0.8.13-fabric");
        assert_eq!(v.file.filename, "sodium-fabric-0.8.13.jar");
        assert_eq!(v.file.size, 1574609);
        assert_eq!(v.file.hashes.get(HashAlgo::Sha512).unwrap().hex(), SHA512);
        assert_eq!(v.game_versions[0].as_str(), "1.21.1");
    }

    #[test]
    fn handles_year_based_game_versions() {
        let v = parse(version_json(
            serde_json::json!({ "game_versions": ["26.3"] }),
        ));
        assert_eq!(v.game_versions[0].as_str(), "26.3");
        assert!(v.supports(&MinecraftVersion::new("26.3"), LoaderKind::Fabric));
    }

    #[test]
    fn picks_the_primary_file_not_the_first() {
        let v = parse(version_json(serde_json::json!({
            "files": [
                { "hashes": {"sha512": SHA512}, "url": "https://cdn.modrinth.com/sources.jar",
                  "filename": "sources.jar", "primary": false, "size": 1 },
                { "hashes": {"sha512": SHA512}, "url": "https://cdn.modrinth.com/mod.jar",
                  "filename": "mod.jar", "primary": true, "size": 2 },
            ]
        })));
        assert_eq!(v.file.filename, "mod.jar");
    }

    #[test]
    fn falls_back_to_the_first_file_when_none_is_primary() {
        let v = parse(version_json(serde_json::json!({
            "files": [
                { "hashes": {"sha512": SHA512}, "url": "https://cdn.modrinth.com/a.jar",
                  "filename": "a.jar", "primary": false, "size": 1 },
            ]
        })));
        assert_eq!(v.file.filename, "a.jar");
    }

    #[test]
    fn refuses_a_version_with_no_files() {
        let wire: WireVersion =
            serde_json::from_value(version_json(serde_json::json!({ "files": [] }))).unwrap();
        assert!(matches!(
            wire.into_version().unwrap_err(),
            ConvertError::NoFile(_)
        ));
    }

    #[test]
    fn refuses_a_file_without_sha512() {
        // sha512 is our content identity; without it we cannot key the cache or the lockfile.
        let wire: WireVersion = serde_json::from_value(version_json(serde_json::json!({
            "files": [{ "hashes": {"sha1": SHA1}, "url": "https://cdn.modrinth.com/a.jar",
                        "filename": "a.jar", "primary": true, "size": 1 }]
        })))
        .unwrap();
        assert!(matches!(
            wire.into_version().unwrap_err(),
            ConvertError::NoSha512 { .. }
        ));
    }

    #[test]
    fn tolerates_a_missing_environment_field() {
        // Older records and some projects carry no `environment`; that is unknown, not a yes.
        let v = parse(version_json(serde_json::json!({})));
        assert_eq!(v.environment, None);
        assert!(v.env_signals().is_empty());
    }

    #[test]
    fn version_environment_becomes_a_signal() {
        let v = parse(version_json(
            serde_json::json!({ "environment": "client_only" }),
        ));
        let sigs = v.env_signals();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].source, EnvSource::RegistryVersionEnv);
        assert_eq!(sigs[0].claim, EnvClaim::ServerUnsupported);
    }

    #[test]
    fn every_documented_environment_value_deserializes() {
        for name in [
            "client_and_server",
            "client_only",
            "client_only_server_optional",
            "singleplayer_only",
            "server_only",
            "server_only_client_optional",
            "dedicated_server_only",
            "client_or_server",
            "client_or_server_prefers_both",
            "unknown",
        ] {
            let v = parse(version_json(serde_json::json!({ "environment": name })));
            assert!(v.environment.is_some(), "{name} should deserialize");
        }
    }

    #[test]
    fn legacy_project_fields_yield_a_weaker_signal() {
        let p: WireProject = serde_json::from_value(serde_json::json!({
            "id": "AANobbMI",
            "slug": "sodium",
            "title": "Sodium",
            "project_type": "mod",
            "client_side": "required",
            "server_side": "unsupported",
        }))
        .unwrap();
        let sigs = p.env_signals();
        assert_eq!(sigs[0].claim, EnvClaim::ServerUnsupported);
        // Weaker than the per-version field, which is newer and more granular.
        assert!(
            sigs[0].weight
                < crate::model::env::weight_for(
                    EnvSource::RegistryVersionEnv,
                    EnvClaim::ServerUnsupported
                )
        );
    }

    #[test]
    fn unknown_legacy_side_produces_no_signal_at_all() {
        let p: WireProject = serde_json::from_value(serde_json::json!({
            "id": "X", "title": "X", "project_type": "mod", "server_side": "unknown",
        }))
        .unwrap();
        assert!(p.env_signals().is_empty());
    }

    #[test]
    fn dependencies_survive_conversion() {
        let v = parse(version_json(serde_json::json!({
            "dependencies": [
                { "project_id": "P8dR8YAK", "dependency_type": "required" },
                { "project_id": "QQQ", "dependency_type": "optional" },
            ]
        })));
        assert_eq!(v.dependencies.len(), 2);
        assert_eq!(v.dependencies[0].dependency_type, DependencyType::Required);
    }

    #[test]
    fn supports_checks_loader_and_game_version_together() {
        let v = parse(version_json(serde_json::json!({
            "game_versions": ["26.3"], "loaders": ["fabric"]
        })));
        assert!(v.supports(&MinecraftVersion::new("26.3"), LoaderKind::Fabric));
        assert!(!v.supports(&MinecraftVersion::new("26.2"), LoaderKind::Fabric));
        assert!(!v.supports(&MinecraftVersion::new("26.3"), LoaderKind::NeoForge));
    }

    #[test]
    fn collections_carry_projects_but_no_versions() {
        // The defining limitation: a collection cannot be installed without being told a
        // Minecraft version and loader.
        let c: WireCollection = serde_json::from_value(serde_json::json!({
            "id": "HQFfBFap",
            "name": "Create Addons",
            "status": "listed",
            "projects": ["3xu2jXe8", "5ZuwMbpk"],
        }))
        .unwrap();
        assert_eq!(c.projects.len(), 2);
        assert_eq!(c.name, "Create Addons");
    }

    #[test]
    fn hashes_are_batched_rather_than_requested_one_by_one() {
        let hashes: Vec<Digest> = (0..1200)
            .map(|i| Digest::new(HashAlgo::Sha512, &format!("{i:0128x}")).unwrap())
            .collect();
        let batches: Vec<&[Digest]> = hash_batches(&hashes).collect();
        assert_eq!(
            batches.len(),
            3,
            "1200 hashes should be 3 requests, not 1200"
        );
        assert!(batches.iter().all(|b| b.len() <= MAX_HASHES_PER_BATCH));
        assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), 1200);
    }

    #[test]
    fn user_agent_follows_the_documented_shape() {
        let ua = user_agent("ptMuta", "0.1.0", "https://github.com/ptMuta/hopper");
        assert_eq!(
            ua,
            "ptMuta/hopper/0.1.0 (+https://github.com/ptMuta/hopper)"
        );
    }
}
