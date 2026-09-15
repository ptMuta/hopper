//! Mojang's piston metadata.
//!
//! Two things come from here, and the second one matters more than it looks.
//!
//! * The vanilla server jar and its sha1.
//! * `javaVersion.majorVersion` — the **authoritative** Java requirement for a given Minecraft
//!   version, published per version by Mojang themselves. Every tool that hardcodes a
//!   "Minecraft 1.x needs Java N" table eventually ships a wrong one; reading it from the
//!   manifest means the answer stays right for versions that did not exist when we shipped.
//!
//! Version ids are opaque strings throughout. Mojang abandoned `1.x` numbering after 1.21.11
//! and now ships year-based ids like `26.3`, so ordering comes from manifest order and never
//! from parsing.

use serde::Deserialize;

use crate::model::{Digest, HashAlgo, MinecraftVersion};

pub const VERSION_MANIFEST: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseType {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    pub id: MinecraftVersion,
    #[serde(rename = "type")]
    pub release_type: ReleaseType,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LatestVersions {
    pub release: MinecraftVersion,
    pub snapshot: MinecraftVersion,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionManifest {
    pub latest: LatestVersions,
    pub versions: Vec<ManifestEntry>,
}

impl VersionManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, MojangError> {
        serde_json::from_slice(bytes).map_err(|e| MojangError::BadJson(e.to_string()))
    }

    pub fn find(&self, id: &MinecraftVersion) -> Option<&ManifestEntry> {
        self.versions.iter().find(|v| v.id == *id)
    }

    pub fn latest_release(&self) -> Option<&ManifestEntry> {
        self.find(&self.latest.release)
    }

    /// Releases only, newest first — manifest order, never a parsed comparison.
    pub fn releases(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.versions
            .iter()
            .filter(|v| v.release_type == ReleaseType::Release)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MojangError {
    #[error("Mojang metadata is not valid JSON: {0}")]
    BadJson(String),
    #[error("Minecraft {0} is not in Mojang's version manifest")]
    UnknownVersion(MinecraftVersion),
    #[error("Minecraft {0} publishes no server download")]
    NoServerJar(MinecraftVersion),
    #[error("Minecraft {0} declares no Java requirement")]
    NoJavaVersion(MinecraftVersion),
    #[error("malformed sha1 in Mojang metadata: {0}")]
    BadHash(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireDownload {
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireDownloads {
    #[serde(default)]
    pub client: Option<WireDownload>,
    #[serde(default)]
    pub server: Option<WireDownload>,
}

/// Mojang's own runtime selector. `component` names the JRE image they ship
/// (`java-runtime-delta`, `java-runtime-epsilon`, …); `major_version` is the number we act on.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireJavaVersion {
    /// Informational only. Mojang's component names are not chronological, so the integer is
    /// what we act on.
    #[serde(default)]
    pub component: Option<String>,
    pub major_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireVersionMeta {
    pub id: MinecraftVersion,
    #[serde(default)]
    pub downloads: Option<WireDownloads>,
    #[serde(default)]
    pub java_version: Option<WireJavaVersion>,
}

/// What a specific Minecraft version needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRelease {
    pub minecraft: MinecraftVersion,
    pub jar_url: String,
    pub jar_sha1: Digest,
    pub jar_size: u64,
    /// Straight from `javaVersion.majorVersion`; never a lookup table.
    pub java_major: u32,
}

impl WireVersionMeta {
    pub fn into_server_release(self) -> Result<ServerRelease, MojangError> {
        let id = self.id;
        let server = self
            .downloads
            .and_then(|d| d.server)
            .ok_or_else(|| MojangError::NoServerJar(id.clone()))?;
        let java_major = self
            .java_version
            .as_ref()
            .map(|j| j.major_version)
            .ok_or_else(|| MojangError::NoJavaVersion(id.clone()))?;
        let jar_sha1 = Digest::new(HashAlgo::Sha1, &server.sha1)
            .map_err(|e| MojangError::BadHash(e.to_string()))?;

        Ok(ServerRelease {
            minecraft: id,
            jar_url: server.url,
            jar_sha1,
            jar_size: server.size,
            java_major,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed manifest that deliberately mixes the legacy and year-based schemes. If anyone
    /// reintroduces a `1.`-prefix assumption, this fixture is what fails.
    fn manifest_json() -> serde_json::Value {
        serde_json::json!({
            "latest": { "release": "26.3", "snapshot": "26.4-pre1" },
            "versions": [
                { "id": "26.4-pre1", "type": "snapshot",
                  "url": "https://piston-meta.mojang.com/v1/packages/aa/26.4-pre1.json" },
                { "id": "26.3", "type": "release",
                  "url": "https://piston-meta.mojang.com/v1/packages/bb/26.3.json" },
                { "id": "26.1", "type": "release",
                  "url": "https://piston-meta.mojang.com/v1/packages/cc/26.1.json" },
                { "id": "1.21.1", "type": "release",
                  "url": "https://piston-meta.mojang.com/v1/packages/dd/1.21.1.json" },
                { "id": "1.16.5", "type": "release",
                  "url": "https://piston-meta.mojang.com/v1/packages/ee/1.16.5.json" }
            ]
        })
    }

    fn version_meta(id: &str, java_major: u32) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "javaVersion": { "component": "java-runtime-epsilon", "majorVersion": java_major },
            "downloads": {
                "server": {
                    "sha1": "aabbccddeeff00112233445566778899aabbccdd",
                    "size": 62294556,
                    "url": "https://piston-data.mojang.com/v1/objects/aa/server.jar"
                }
            }
        })
    }

    #[test]
    fn parses_the_manifest_and_finds_the_latest_release() {
        let m = VersionManifest::parse(manifest_json().to_string().as_bytes()).unwrap();
        assert_eq!(m.latest.release.as_str(), "26.3");
        assert_eq!(m.latest_release().unwrap().id.as_str(), "26.3");
    }

    #[test]
    fn handles_both_version_schemes_without_parsing_them() {
        let m = VersionManifest::parse(manifest_json().to_string().as_bytes()).unwrap();
        for id in ["26.3", "26.1", "1.21.1", "1.16.5"] {
            assert!(
                m.find(&MinecraftVersion::new(id)).is_some(),
                "{id} should be findable"
            );
        }
    }

    #[test]
    fn releases_excludes_snapshots_and_keeps_manifest_order() {
        let m = VersionManifest::parse(manifest_json().to_string().as_bytes()).unwrap();
        let ids: Vec<&str> = m.releases().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["26.3", "26.1", "1.21.1", "1.16.5"]);
    }

    #[test]
    fn an_unknown_version_is_simply_absent() {
        let m = VersionManifest::parse(manifest_json().to_string().as_bytes()).unwrap();
        assert!(m.find(&MinecraftVersion::new("99.9")).is_none());
    }

    #[test]
    fn reads_the_java_requirement_from_the_version_itself() {
        // The point of this module: no hardcoded Minecraft-to-Java table anywhere.
        for (id, major) in [("26.3", 25), ("1.21.1", 21), ("1.16.5", 8)] {
            let meta: WireVersionMeta = serde_json::from_value(version_meta(id, major)).unwrap();
            let rel = meta.into_server_release().unwrap();
            assert_eq!(rel.java_major, major, "{id}");
            assert_eq!(rel.minecraft.as_str(), id);
        }
    }

    #[test]
    fn extracts_the_server_jar_and_its_hash() {
        let meta: WireVersionMeta = serde_json::from_value(version_meta("26.3", 25)).unwrap();
        let rel = meta.into_server_release().unwrap();
        assert_eq!(rel.jar_size, 62294556);
        assert_eq!(rel.jar_sha1.algo(), HashAlgo::Sha1);
        assert!(rel.jar_url.ends_with("server.jar"));
    }

    #[test]
    fn a_client_only_version_is_refused_with_a_clear_reason() {
        // Some very old versions publish no server download at all.
        let meta: WireVersionMeta = serde_json::from_value(serde_json::json!({
            "id": "1.5.2",
            "javaVersion": { "majorVersion": 8 },
            "downloads": { "client": { "sha1": "aa", "size": 1, "url": "https://x/client.jar" } }
        }))
        .unwrap();
        assert!(matches!(
            meta.into_server_release().unwrap_err(),
            MojangError::NoServerJar(_)
        ));
    }

    #[test]
    fn a_version_without_a_java_requirement_is_refused_rather_than_guessed() {
        let meta: WireVersionMeta = serde_json::from_value(serde_json::json!({
            "id": "1.2.5",
            "downloads": { "server": { "sha1": "aabbccddeeff00112233445566778899aabbccdd",
                                       "size": 1, "url": "https://x/server.jar" } }
        }))
        .unwrap();
        assert!(matches!(
            meta.into_server_release().unwrap_err(),
            MojangError::NoJavaVersion(_)
        ));
    }

    #[test]
    fn a_malformed_hash_is_rejected() {
        let mut v = version_meta("26.3", 25);
        v["downloads"]["server"]["sha1"] = serde_json::json!("not-a-hash");
        let meta: WireVersionMeta = serde_json::from_value(v).unwrap();
        assert!(matches!(
            meta.into_server_release().unwrap_err(),
            MojangError::BadHash(_)
        ));
    }

    #[test]
    fn rejects_non_json() {
        assert!(matches!(
            VersionManifest::parse(b"nope").unwrap_err(),
            MojangError::BadJson(_)
        ));
    }
}
