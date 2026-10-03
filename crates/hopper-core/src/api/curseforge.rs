//! CurseForge's API, as far as installing a server pack needs it.
//!
//! Two differences from Modrinth shape everything here:
//!
//! - Every request needs an API key. It is the operator's own, never embedded, and the client
//!   that carries it can reach only `api.curseforge.com` (see
//!   [`crate::net::allowlist::CURSEFORGE_API_HOSTS`]). Downloads go through a separate client
//!   that has no key at all.
//! - Files carry sha1 and md5, never sha512. sha1 is the verify digest; the store's own sha512
//!   becomes the content digest once a file is downloaded.
//!
//! A modpack's files come in pairs: the client pack (a `manifest.json` plus overrides) and,
//! when the author published one, a server pack linked from it by `serverPackFileId`. The
//! server pack is what the author decided belongs on a server, so it is preferred whenever it
//! exists.

use serde::{Deserialize, Serialize};

use crate::model::{Digest, HashAlgo, LoaderKind, MinecraftVersion};
use crate::net::{HttpClient, HttpError};

pub const API_V1: &str = "https://api.curseforge.com/v1";
/// The header the API key travels in.
pub const KEY_HEADER: &str = "x-api-key";
/// Minecraft, in CurseForge's game numbering.
pub const GAME_MINECRAFT: u32 = 432;

/// Project classes, which decide where a file goes on disk.
pub const CLASS_MOD: u32 = 6;
pub const CLASS_RESOURCE_PACK: u32 = 12;
pub const CLASS_MODPACK: u32 = 4471;
pub const CLASS_SHADER: u32 = 6552;

/// CurseForge's hash algorithm numbering.
const ALGO_SHA1: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CurseForgeError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("{slug:?} was not found on CurseForge")]
    NotFound { slug: String },
    #[error("{slug:?} has no files")]
    NoFiles { slug: String },
    #[error("{slug:?} has no file {version:?}")]
    NoSuchVersion { slug: String, version: String },
    #[error("{slug:?} has no file for Minecraft {mc}")]
    NoVersionForMinecraft { slug: String, mc: MinecraftVersion },
    #[error("CurseForge file {file} publishes no sha1, so it cannot be verified")]
    NoSha1 { file: u64 },
    #[error("CurseForge returned something that is not a valid response: {0}")]
    Malformed(String),
    #[error("not a CurseForge modpack manifest: {0}")]
    BadManifest(String),
    #[error("the pack's mod loader {0:?} is not one hopper knows")]
    UnknownLoader(String),
}

/// Every response is wrapped in `{"data": ...}`.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    #[serde(default)]
    total_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireMod {
    pub id: u64,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub class_id: Option<u32>,
    /// False when the author has opted out of third-party downloads. Their files then come
    /// back with a null `downloadUrl`.
    #[serde(default)]
    pub allow_mod_distribution: Option<bool>,
    #[serde(default)]
    pub links: WireLinks,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireLinks {
    #[serde(default)]
    pub website_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireHash {
    pub value: String,
    pub algo: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseType {
    Release,
    Beta,
    Alpha,
}

impl ReleaseType {
    fn from_wire(n: u32) -> Option<Self> {
        match n {
            1 => Some(Self::Release),
            2 => Some(Self::Beta),
            3 => Some(Self::Alpha),
            _ => None,
        }
    }
}

impl std::fmt::Display for ReleaseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Release => "release",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFile {
    pub id: u64,
    #[serde(default)]
    pub mod_id: u64,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub release_type: u32,
    /// RFC 3339, which sorts correctly as a string.
    #[serde(default)]
    pub file_date: String,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub hashes: Vec<WireHash>,
    #[serde(default)]
    pub file_length: Option<u64>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub server_pack_file_id: Option<u64>,
    #[serde(default)]
    pub is_server_pack: bool,
    #[serde(default)]
    pub is_available: Option<bool>,
}

impl WireFile {
    pub fn release(&self) -> Option<ReleaseType> {
        ReleaseType::from_wire(self.release_type)
    }

    /// The digest to verify a download against. md5 is published too and deliberately ignored.
    pub fn sha1(&self) -> Result<Digest, CurseForgeError> {
        self.hashes
            .iter()
            .filter(|h| h.algo == ALGO_SHA1)
            .find_map(|h| Digest::new(HashAlgo::Sha1, &h.value).ok())
            .ok_or(CurseForgeError::NoSha1 { file: self.id })
    }

    /// The linked server pack, if the author published one. Zero means none.
    pub fn server_pack(&self) -> Option<u64> {
        self.server_pack_file_id.filter(|id| *id != 0)
    }

    /// Whether the file is tagged as running on a client, a server, or both.
    ///
    /// CurseForge puts environment tags in `gameVersions` beside Minecraft versions and loader
    /// names. Absent tags say nothing, which is the common case.
    pub fn environment_tags(&self) -> (bool, bool) {
        let has = |t: &str| self.game_versions.iter().any(|g| g.eq_ignore_ascii_case(t));
        (has("Client"), has("Server"))
    }

    /// The label an operator would recognise and can pass back as `[VERSION]`.
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.file_name
        } else {
            &self.display_name
        }
    }
}

/// Look a modpack up by slug.
pub async fn find_modpack(client: &HttpClient, slug: &str) -> Result<WireMod, CurseForgeError> {
    if let Ok(id) = slug.parse::<u64>() {
        let result: Envelope<WireMod> = client.get_json(&format!("{API_V1}/mods/{id}")).await?;
        if result.data.class_id != Some(CLASS_MODPACK) {
            return Err(CurseForgeError::NotFound {
                slug: slug.to_owned(),
            });
        }
        return Ok(result.data);
    }
    let url = format!(
        "{API_V1}/mods/search?gameId={GAME_MINECRAFT}&classId={CLASS_MODPACK}&slug={}",
        urlencode(slug)
    );
    let found: Envelope<Vec<WireMod>> = client.get_json(&url).await?;
    // The slug filter is exact on CurseForge's side, but checking costs nothing and a
    // fuzzy match installing the wrong pack would be bad.
    found
        .data
        .into_iter()
        .find(|m| m.slug.eq_ignore_ascii_case(slug))
        .ok_or_else(|| CurseForgeError::NotFound {
            slug: slug.to_owned(),
        })
}

/// Every file of a project, newest first.
pub async fn list_files(
    client: &HttpClient,
    mod_id: u64,
) -> Result<Vec<WireFile>, CurseForgeError> {
    const PAGE: u64 = 50;
    // CurseForge refuses to page past this, and no real pack has that many files.
    const MAX: u64 = 10_000;

    let mut out = Vec::new();
    let mut index = 0;
    loop {
        let url = format!("{API_V1}/mods/{mod_id}/files?index={index}&pageSize={PAGE}");
        let page: Envelope<Vec<WireFile>> = client.get_json(&url).await?;
        let n = page.data.len() as u64;
        out.extend(page.data);
        index += n;
        let total = page.pagination.map_or(0, |p| p.total_count);
        if n == 0 || index >= total || index >= MAX {
            break;
        }
    }
    out.sort_by(|a, b| b.file_date.cmp(&a.file_date));
    Ok(out)
}

pub async fn get_file(
    client: &HttpClient,
    mod_id: u64,
    file_id: u64,
) -> Result<WireFile, CurseForgeError> {
    let url = format!("{API_V1}/mods/{mod_id}/files/{file_id}");
    let file: Envelope<WireFile> = client.get_json(&url).await?;
    Ok(file.data)
}

/// Resolve many file IDs at once, as a manifest lists them.
pub async fn get_files(
    client: &HttpClient,
    file_ids: &[u64],
) -> Result<Vec<WireFile>, CurseForgeError> {
    let mut out = Vec::with_capacity(file_ids.len());
    for chunk in file_ids.chunks(500) {
        let body = serde_json::json!({ "fileIds": chunk });
        let page: Envelope<Vec<WireFile>> = client
            .post_json(&format!("{API_V1}/mods/files"), &body)
            .await?;
        out.extend(page.data);
    }
    Ok(out)
}

/// Resolve many projects at once, for their class and distribution policy.
pub async fn get_mods(
    client: &HttpClient,
    mod_ids: &[u64],
) -> Result<Vec<WireMod>, CurseForgeError> {
    let mut out = Vec::with_capacity(mod_ids.len());
    for chunk in mod_ids.chunks(500) {
        let body = serde_json::json!({ "modIds": chunk });
        let page: Envelope<Vec<WireMod>> =
            client.post_json(&format!("{API_V1}/mods"), &body).await?;
        out.extend(page.data);
    }
    Ok(out)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Pick the client-pack file to install.
///
/// Server packs are listed as files of the project too, and are never the thing to choose:
/// they are reached through the client file's `serverPackFileId`. Otherwise the rule matches
/// Modrinth's: newest release, then beta, then alpha.
pub fn choose_file<'a>(
    files: &'a [WireFile],
    slug: &str,
    wanted: Option<&str>,
    mc: Option<&MinecraftVersion>,
) -> Result<&'a WireFile, CurseForgeError> {
    let candidates: Vec<&WireFile> = files
        .iter()
        .filter(|f| !f.is_server_pack && f.is_available != Some(false))
        .collect();
    if candidates.is_empty() {
        return Err(CurseForgeError::NoFiles {
            slug: slug.to_owned(),
        });
    }

    if let Some(want) = wanted {
        return candidates
            .iter()
            .copied()
            .find(|f| {
                f.id.to_string() == want
                    || f.display_name == want
                    || f.file_name == want
                    || f.file_name.strip_suffix(".zip") == Some(want)
            })
            .ok_or_else(|| CurseForgeError::NoSuchVersion {
                slug: slug.to_owned(),
                version: want.to_owned(),
            });
    }

    let matches_mc = |f: &WireFile| match mc {
        Some(want) => f.game_versions.iter().any(|g| g == want.as_str()),
        None => true,
    };
    let newest = |kind: Option<ReleaseType>| {
        candidates
            .iter()
            .copied()
            .filter(|f| matches_mc(f) && (kind.is_none() || f.release() == kind))
            .max_by(|a, b| a.file_date.cmp(&b.file_date))
    };

    for kind in [ReleaseType::Release, ReleaseType::Beta, ReleaseType::Alpha] {
        if let Some(f) = newest(Some(kind)) {
            return Ok(f);
        }
    }
    newest(None).ok_or_else(|| match mc {
        Some(mc) => CurseForgeError::NoVersionForMinecraft {
            slug: slug.to_owned(),
            mc: mc.clone(),
        },
        None => CurseForgeError::NoFiles {
            slug: slug.to_owned(),
        },
    })
}

/// `manifest.json` from a CurseForge client pack.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub minecraft: ManifestMinecraft,
    #[serde(default)]
    pub manifest_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    /// The override folder's name. Almost always `overrides`, but the format lets it vary.
    #[serde(default = "default_overrides")]
    pub overrides: String,
}

fn default_overrides() -> String {
    "overrides".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestMinecraft {
    pub version: String,
    #[serde(default)]
    pub mod_loaders: Vec<ManifestLoader>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestLoader {
    pub id: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ManifestFile {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, CurseForgeError> {
        let m: Self = serde_json::from_slice(bytes)
            .map_err(|e| CurseForgeError::BadManifest(e.to_string()))?;
        if !m.manifest_type.is_empty() && m.manifest_type != "minecraftModpack" {
            return Err(CurseForgeError::BadManifest(format!(
                "manifestType is {:?}",
                m.manifest_type
            )));
        }
        if m.overrides.is_empty() || m.overrides.contains(['/', '\\']) || m.overrides == ".." {
            return Err(CurseForgeError::BadManifest(format!(
                "overrides folder {:?} is not a plain folder name",
                m.overrides
            )));
        }
        Ok(m)
    }

    pub fn minecraft(&self) -> MinecraftVersion {
        MinecraftVersion::new(&self.minecraft.version)
    }

    /// The primary loader and its version, as `forge-47.2.0` or `neoforge-21.1.95` spell it.
    pub fn loader(&self) -> Result<(LoaderKind, Option<String>), CurseForgeError> {
        let Some(entry) = self
            .minecraft
            .mod_loaders
            .iter()
            .find(|l| l.primary)
            .or_else(|| self.minecraft.mod_loaders.first())
        else {
            return Ok((LoaderKind::Vanilla, None));
        };
        parse_loader_id(&entry.id)
    }
}

/// `forge-47.2.0` -> Forge 47.2.0. The kind is everything before the first dash.
pub fn parse_loader_id(id: &str) -> Result<(LoaderKind, Option<String>), CurseForgeError> {
    let (kind, version) = id.split_once('-').unwrap_or((id, ""));
    let kind = match kind.to_ascii_lowercase().as_str() {
        "forge" => LoaderKind::Forge,
        "neoforge" => LoaderKind::NeoForge,
        "fabric" => LoaderKind::Fabric,
        "quilt" => LoaderKind::Quilt,
        _ => return Err(CurseForgeError::UnknownLoader(id.to_owned())),
    };
    // NeoForge's 1.20.1 builds are spelled `neoforge-1.20.1-47.1.106`; the loader version is
    // the part after the Minecraft version.
    let version = match version.rsplit_once('-') {
        Some((mc, v)) if kind == LoaderKind::NeoForge && mc.starts_with("1.") => v,
        _ => version,
    };
    Ok((kind, (!version.is_empty()).then(|| version.to_owned())))
}

/// Where a project's files belong, by its class.
pub fn dir_for_class(class_id: Option<u32>) -> Option<&'static str> {
    match class_id {
        Some(CLASS_MOD) | None => Some("mods"),
        Some(CLASS_RESOURCE_PACK) => Some("resourcepacks"),
        Some(CLASS_SHADER) => Some("shaderpacks"),
        // Worlds, modpacks and the like have no place in a server's file tree.
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: u64, release: u32, date: &str, versions: &[&str]) -> WireFile {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "displayName": format!("Pack-{id}"),
            "fileName": format!("Pack-{id}.zip"),
            "releaseType": release,
            "fileDate": date,
            "gameVersions": versions,
            "hashes": [
                {"value": "da39a3ee5e6b4b0d3255bfef95601890afd80709", "algo": 1},
                {"value": "d41d8cd98f00b204e9800998ecf8427e", "algo": 2}
            ],
            "serverPackFileId": id + 1000,
        }))
        .unwrap()
    }

    #[test]
    fn reads_a_real_file_response() {
        // Trimmed from DeceasedCraft's server pack, as the API returned it.
        let f: Envelope<WireFile> = serde_json::from_value(serde_json::json!({"data": {
            "id": 8448984, "gameId": 432, "modId": 490660, "isAvailable": true,
            "displayName": "DeceasedCraft_Server_Beta_DH_Edition_5.10.17",
            "fileName": "DeceasedCraft_Server_Beta_DH_Edition_5.10.17.zip",
            "releaseType": 3, "fileStatus": 4,
            "hashes": [
                {"value": "b16461659e27bcbce268da4264806055e4444a8e", "algo": 1},
                {"value": "ddf67fa661f4c4f800592966ca4287d3", "algo": 2}
            ],
            "fileDate": "2026-07-17T04:04:39.777Z", "fileLength": 604626328,
            "downloadUrl": "https://edge.forgecdn.net/files/8448/984/DeceasedCraft_Server_Beta_DH_Edition_5.10.17.zip",
            "gameVersions": ["1.20.1", "Forge"],
            "parentProjectFileId": 8448903, "isServerPack": true, "serverPackFileId": null
        }}))
        .unwrap();
        let f = f.data;
        assert!(f.is_server_pack);
        assert_eq!(f.release(), Some(ReleaseType::Alpha));
        assert_eq!(
            f.sha1().unwrap().hex(),
            "b16461659e27bcbce268da4264806055e4444a8e"
        );
        assert_eq!(f.server_pack(), None);
    }

    #[test]
    fn a_file_without_sha1_cannot_be_verified() {
        let mut f = file(1, 1, "2026-01-01", &[]);
        f.hashes.retain(|h| h.algo != ALGO_SHA1);
        assert!(matches!(f.sha1(), Err(CurseForgeError::NoSha1 { file: 1 })));
    }

    #[test]
    fn zero_means_no_server_pack() {
        let mut f = file(1, 1, "2026-01-01", &[]);
        f.server_pack_file_id = Some(0);
        assert_eq!(f.server_pack(), None);
    }

    #[test]
    fn a_newer_alpha_loses_to_an_older_release() {
        // DeceasedCraft ships an alpha "DH Edition" beside each release; the release wins.
        let files = vec![
            file(3, 3, "2026-07-17T04:04:00Z", &["1.20.1"]),
            file(2, 1, "2026-07-16T00:00:00Z", &["1.20.1"]),
            file(1, 1, "2026-01-01T00:00:00Z", &["1.20.1"]),
        ];
        assert_eq!(choose_file(&files, "p", None, None).unwrap().id, 2);
    }

    #[test]
    fn server_packs_are_never_chosen_directly() {
        let mut sp = file(9, 1, "2027-01-01T00:00:00Z", &["1.20.1"]);
        sp.is_server_pack = true;
        let files = vec![sp, file(1, 2, "2026-01-01T00:00:00Z", &["1.20.1"])];
        assert_eq!(choose_file(&files, "p", None, None).unwrap().id, 1);
    }

    #[test]
    fn minecraft_filter_and_explicit_versions() {
        let files = vec![
            file(2, 1, "2026-02-01T00:00:00Z", &["1.21.1"]),
            file(1, 1, "2026-01-01T00:00:00Z", &["1.20.1"]),
        ];
        let mc = MinecraftVersion::new("1.20.1");
        assert_eq!(choose_file(&files, "p", None, Some(&mc)).unwrap().id, 1);
        assert_eq!(choose_file(&files, "p", Some("2"), None).unwrap().id, 2);
        assert_eq!(
            choose_file(&files, "p", Some("Pack-1"), None).unwrap().id,
            1
        );
        assert_eq!(
            choose_file(&files, "p", Some("Pack-1.zip"), None)
                .unwrap()
                .id,
            1
        );
        assert!(matches!(
            choose_file(&files, "p", Some("nope"), None),
            Err(CurseForgeError::NoSuchVersion { .. })
        ));
        assert!(matches!(
            choose_file(&files, "p", None, Some(&MinecraftVersion::new("1.7.10"))),
            Err(CurseForgeError::NoVersionForMinecraft { .. })
        ));
        assert!(matches!(
            choose_file(&[], "p", None, None),
            Err(CurseForgeError::NoFiles { .. })
        ));
    }

    #[test]
    fn loader_ids_parse_into_kind_and_version() {
        assert_eq!(
            parse_loader_id("forge-47.2.0").unwrap(),
            (LoaderKind::Forge, Some("47.2.0".into()))
        );
        assert_eq!(
            parse_loader_id("neoforge-21.1.95").unwrap(),
            (LoaderKind::NeoForge, Some("21.1.95".into()))
        );
        assert_eq!(
            parse_loader_id("neoforge-1.20.1-47.1.106").unwrap(),
            (LoaderKind::NeoForge, Some("47.1.106".into()))
        );
        assert_eq!(
            parse_loader_id("fabric-0.16.5").unwrap(),
            (LoaderKind::Fabric, Some("0.16.5".into()))
        );
        assert!(parse_loader_id("liteloader-1.0").is_err());
    }

    #[test]
    fn manifests_parse_and_name_their_loader() {
        let m = Manifest::parse(
            serde_json::json!({
                "minecraft": {"version": "1.20.1", "modLoaders": [
                    {"id": "forge-47.4.0", "primary": true}
                ]},
                "manifestType": "minecraftModpack",
                "manifestVersion": 1,
                "name": "DeceasedCraft",
                "version": "5.10.17",
                "files": [{"projectID": 238222, "fileID": 4712345, "required": true}],
                "overrides": "overrides"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(m.minecraft().as_str(), "1.20.1");
        assert_eq!(
            m.loader().unwrap(),
            (LoaderKind::Forge, Some("47.4.0".into()))
        );
        assert_eq!(m.files.len(), 1);
    }

    #[test]
    fn a_manifest_cannot_point_overrides_outside_the_archive_root() {
        for bad in ["../x", "a/b", ""] {
            let body = serde_json::json!({
                "minecraft": {"version": "1.20.1"},
                "overrides": bad
            });
            assert!(
                Manifest::parse(body.to_string().as_bytes()).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn classes_map_to_directories() {
        assert_eq!(dir_for_class(Some(CLASS_MOD)), Some("mods"));
        assert_eq!(dir_for_class(None), Some("mods"));
        assert_eq!(
            dir_for_class(Some(CLASS_RESOURCE_PACK)),
            Some("resourcepacks")
        );
        assert_eq!(dir_for_class(Some(CLASS_SHADER)), Some("shaderpacks"));
        assert_eq!(dir_for_class(Some(17)), None);
    }

    #[test]
    fn environment_tags_are_read_from_game_versions() {
        let f = file(1, 1, "2026", &["1.20.1", "Forge", "Client"]);
        assert_eq!(f.environment_tags(), (true, false));
    }
}
