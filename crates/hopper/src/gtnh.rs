//! Native GTNH assembly. Every artifact is staged before reconciliation; upstream scripts
//! and the destructive nightly updater are never executed.
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use anyhow::{Context, Result, bail, ensure};
use hopper_core::cache::BlobStore;
use hopper_core::loader::LaunchProfile;
use hopper_core::model::{Digest, LoaderKind, MinecraftVersion, RelPath};
use hopper_core::net::{HostAllowlist, HttpClient};
use hopper_core::source::mrpack::{self, Mrpack, MrpackIndex, OverrideEntry, OverrideKind};
use serde::Deserialize;
use serde_json::Value;

pub const CATALOG: &str = "https://downloads.gtnewhorizons.com/versions.json";
const REPO: &str = "GTNewHorizons/DreamAssemblerXXL";

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub title: String,
    #[serde(rename = "releaseDate")]
    pub date: String,
    #[serde(rename = "maxJavaVersion")]
    pub max_java: u32,
    pub server: ServerUrls,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerUrls {
    #[serde(rename = "java8Url")]
    pub legacy: Option<String>,
    #[serde(rename = "java17_2XUrl")]
    pub modern: Option<String>,
}

pub struct Resolved {
    pub pack: Mrpack,
    pub content: BTreeMap<RelPath, (Digest, u64)>,
    pub java: u32,
    pub launch: LaunchProfile,
}

pub fn choose<'a>(
    releases: &'a BTreeMap<String, Release>,
    channel: &str,
    version: Option<&str>,
) -> Result<(&'a str, &'a Release)> {
    if let Some(version) = version {
        let (name, release) = releases
            .get_key_value(version)
            .context("GTNH version not found")?;
        return Ok((name, release));
    }
    let title = match channel {
        "stable" => "Stable release",
        "testing" => "Beta release",
        _ => bail!("{channel:?} is not a published GTNH channel"),
    };
    releases
        .iter()
        .filter(|(name, r)| {
            r.title == title
                && name.chars().next().is_some_and(|c| c.is_ascii_digit())
                && (r.server.modern.is_some() || r.server.legacy.is_some())
        })
        .max_by(|(a, x), (b, y)| {
            x.date
                .cmp(&y.date)
                .then_with(|| natural(a).cmp(&natural(b)))
        })
        .map(|(name, release)| (name.as_str(), release))
        .context("no GTNH release in this channel")
}

pub(crate) fn natural(version: &str) -> (Vec<u32>, u8, u32) {
    let nums: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect();
    let stage = if version.contains("RC") || version.contains("rc") {
        2
    } else if version.contains("beta") {
        1
    } else if version.contains("alpha") {
        0
    } else {
        3
    };
    (
        nums.iter().take(3).copied().collect(),
        stage,
        nums.get(3).copied().unwrap_or(0),
    )
}

/// Exact version/snapshot artifact URLs are immutable inputs. Cache their local digests,
/// still verifying the blob on reuse; never cache floating metadata this way.
async fn artifact(
    client: &HttpClient,
    store: &BlobStore,
    url: &str,
) -> Result<hopper_core::cache::Blob> {
    artifact_verified(client, store, url, None).await
}

async fn artifact_verified(
    client: &HttpClient,
    store: &BlobStore,
    url: &str,
    expected: Option<&Digest>,
) -> Result<hopper_core::cache::Blob> {
    let mut hasher = hopper_core::MultiHasher::new();
    hasher.update(url.as_bytes());
    let name = hasher.finish().1.to_string().replace(':', "-");
    let directory = store.root().join("gtnh-urls");
    let metadata = directory.join(name);
    if let Ok(bytes) = std::fs::read(&metadata) {
        if let Ok((cached_url, digest)) = serde_json::from_slice::<(String, Digest)>(&bytes) {
            if cached_url == url {
                if let Some(blob) = store.get_verified(&digest)? {
                    if let Some(expected) = expected {
                        let mut hasher = hopper_core::MultiHasher::new().with_sha256();
                        let mut file = std::fs::File::open(&blob.path)?;
                        let mut buffer = [0u8; 65536];
                        loop {
                            let count = file.read(&mut buffer)?;
                            if count == 0 {
                                break;
                            }
                            hasher.update(&buffer[..count]);
                        }
                        let (hashes, _) = hasher.finish();
                        ensure!(
                            hashes.get(expected.algo()) == Some(expected),
                            "cached GTNH artifact does not match published checksum"
                        );
                    }
                    return Ok(blob);
                }
            }
        }
    }
    let blob = client
        .fetch_to_store(&[url.to_owned()], expected, None, store)
        .await?;
    std::fs::create_dir_all(directory)?;
    hopper_core::fs::write_atomic(&metadata, &serde_json::to_vec(&(url, &blob.digest))?, false)?;
    Ok(blob)
}

fn asset_digest(asset: &Value) -> Result<Option<Digest>> {
    asset
        .get("digest")
        .and_then(Value::as_str)
        .map(Digest::parse)
        .transpose()
        .map_err(Into::into)
}

pub fn java_major(max: u32, requested: Option<u32>) -> Result<u32> {
    let major =
        requested.unwrap_or_else(|| [25, 21, 17, 8].into_iter().find(|v| *v <= max).unwrap_or(8));
    ensure!(
        [8, 17, 21, 25].contains(&major) && major <= max,
        "GTNH does not support Java {major} for this release"
    );
    Ok(major)
}

pub async fn resolve(
    channel: &str,
    version: Option<&str>,
    requested_java: Option<u32>,
    store: &BlobStore,
) -> Result<Resolved> {
    let snapshot = version.filter(|v| v.starts_with("daily:") || v.starts_with("experimental:"));
    let channel = snapshot
        .and_then(|v| v.split(':').next())
        .unwrap_or(channel);
    let client = HttpClient::new(&crate::user_agent(), HostAllowlist::gtnh())?;
    let mut content = BTreeMap::new();
    let (label, major) = if matches!(channel, "daily" | "experimental") {
        ensure!(
            version.is_none() || snapshot.is_some(),
            "daily/experimental use immutable snapshots, not release version pins"
        );
        let major = java_major(25, requested_java)?;
        ensure!(major != 8, "daily and experimental require modern Java");
        let reference = snapshot
            .and_then(|v| v.rsplit(':').next())
            .unwrap_or("master");
        ensure!(
            reference == "master"
                || (reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit())),
            "invalid GTNH snapshot commit"
        );
        let commit: Value = client
            .get_json(&format!(
                "https://api.github.com/repos/{REPO}/commits/{reference}"
            ))
            .await?;
        let sha = string(&commit, "sha")?;
        ensure!(
            sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "invalid upstream commit"
        );
        let raw = format!("https://raw.githubusercontent.com/{REPO}/{sha}");
        let manifest: Value = client
            .get_json(&format!("{raw}/releases/manifests/{channel}.json"))
            .await?;
        let assets: Value = client.get_json(&format!("{raw}/gtnh-assets.json")).await?;
        assemble_mods(&client, store, &manifest, &assets, &mut content).await?;
        let config = string(&manifest, "config")?;
        if let Some(snapshot) = snapshot {
            ensure!(
                snapshot == format!("{channel}:{config}:{sha}"),
                "snapshot label does not match its pinned manifest"
            );
        }
        let config_release: Value = client.get_json(&format!("https://api.github.com/repos/GTNewHorizons/GT-New-Horizons-Modpack/releases/tags/{config}")).await?;
        let asset = config_release["assets"]
            .as_array()
            .context("configuration release has no assets")?
            .iter()
            .find(|a| a["name"].as_str().is_some_and(|s| s.ends_with(".zip")))
            .context("configuration ZIP not found")?;
        let bytes = std::fs::read(
            artifact_verified(
                &client,
                store,
                string(asset, "browser_download_url")?,
                asset_digest(asset)?.as_ref(),
            )
            .await?
            .path,
        )?;
        content.extend(archive(&bytes, store)?);
        let tree: Value = client
            .get_json(&format!(
                "https://api.github.com/repos/{REPO}/git/trees/{sha}?recursive=1"
            ))
            .await?;
        ensure!(
            tree["truncated"] != true,
            "upstream bootstrap tree is truncated"
        );
        for entry in tree["tree"].as_array().context("missing bootstrap tree")? {
            if entry["type"] != "blob" {
                continue;
            }
            let path = string(entry, "path")?;
            let Some(relative) = path.strip_prefix("server_assets/forge/") else {
                continue;
            };
            if !wanted(relative) {
                continue;
            }
            let blob = artifact(&client, store, &format!("{raw}/{path}")).await?;
            content.insert(RelPath::parse(relative)?, (blob.digest, blob.size));
        }
        // Match argument syntax to the mod actually selected by the manifest, not master.
        let lwjgl = string(&manifest["github_mods"]["lwjgl3ify"], "version")?;
        let args = std::fs::read(artifact(&client, store, &format!("https://raw.githubusercontent.com/GTNewHorizons/lwjgl3ify/{lwjgl}/java9args.txt")).await?.path)?;
        let blob = store.insert_bytes(&args, None)?;
        content.insert(RelPath::parse("java9args.txt")?, (blob.digest, blob.size));
        (format!("{channel}:{config}:{sha}"), major)
    } else {
        let releases: BTreeMap<String, Release> = client.get_json(CATALOG).await?;
        let (label, release) = choose(&releases, channel, version)?;
        let major = java_major(release.max_java, requested_java)?;
        let url = if major == 8 {
            &release.server.legacy
        } else {
            &release.server.modern
        }
        .as_deref()
        .context("this GTNH release has no requested Java distribution")?;
        let bytes = std::fs::read(artifact(&client, store, url).await?.path)?;
        content = archive(&bytes, store)?;
        (label.to_owned(), major)
    };
    let jar = if major == 8 {
        content
            .keys()
            .find(|p| {
                !p.as_str().contains('/')
                    && p.as_str().starts_with("forge-")
                    && p.as_str().ends_with(".jar")
            })
            .cloned()
            .context("GTNH bundle has no Forge launcher")?
    } else {
        for path in ["java9args.txt", "lwjgl3ify-forgePatches.jar"] {
            ensure!(
                content.contains_key(&RelPath::parse(path)?),
                "GTNH bootstrap is missing {path}"
            );
        }
        ensure!(
            content.keys().any(|p| p.starts_with_dir("libraries")),
            "GTNH bundle has no libraries"
        );
        RelPath::parse("lwjgl3ify-forgePatches.jar")?
    };
    let launch = LaunchProfile::ExecutableJar {
        jar,
        jvm_args: if major == 8 {
            vec!["-Dfml.readTimeout=180".into()]
        } else {
            vec![
                "@java9args.txt".into(),
                "-Dfml.readTimeout=180".into(),
                "-Duser.language=en".into(),
            ]
        },
        program_args: vec!["nogui".into()],
    };
    let overrides = content
        .iter()
        .map(|(path, (_, size))| OverrideEntry {
            path: path.clone(),
            kind: OverrideKind::Server,
            zip_name: path.to_string(),
            size: *size,
        })
        .collect();
    Ok(Resolved {
        pack: Mrpack {
            index: MrpackIndex {
                name: "GregTech: New Horizons".into(),
                version_id: label,
                summary: None,
                minecraft: MinecraftVersion::new("1.7.10"),
                loader: LoaderKind::Forge,
                loader_version: Some("10.13.4.1614".into()),
                files: vec![],
            },
            overrides,
        },
        content,
        java: major,
        launch,
    })
}

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("upstream metadata missing {key}"))
}

pub fn server_side(side: &str) -> Result<bool> {
    match side {
        "BOTH" | "SERVER" | "BOTH_JAVA9" | "SERVER_JAVA9" => Ok(true),
        "CLIENT" | "NONE" | "CLIENT_JAVA9" | "CLIENT_JAVA8" | "BOTH_JAVA8" | "SERVER_JAVA8" => {
            Ok(false)
        }
        _ => bail!("unknown GTNH side {side:?}"),
    }
}

async fn assemble_mods(
    client: &HttpClient,
    store: &BlobStore,
    manifest: &Value,
    assets: &Value,
    content: &mut BTreeMap<RelPath, (Digest, u64)>,
) -> Result<()> {
    let catalog = assets["mods"]
        .as_array()
        .context("missing GTNH asset catalog")?;
    for group in ["github_mods", "external_mods"] {
        for (name, selected) in manifest[group]
            .as_object()
            .context("missing manifest mod group")?
        {
            if !server_side(string(selected, "side")?)? {
                continue;
            }
            let version = string(selected, "version")?;
            let entry = catalog
                .iter()
                .find(|e| e["name"] == name.as_str())
                .with_context(|| format!("{name} missing from catalog"))?;
            let asset = entry["versions"]
                .as_array()
                .context("missing asset versions")?
                .iter()
                .find(|a| a["version_tag"] == version)
                .with_context(|| format!("{name}@{version} missing from pinned catalog"))?;
            let filename = string(asset, "filename")?;
            ensure!(
                !filename.contains('/') && !filename.contains('\\'),
                "invalid mod filename"
            );
            let url = if entry["source"] == "curse" {
                string(asset, "download_url")?
            } else {
                string(asset, "browser_download_url")?
            };
            let blob = artifact_verified(client, store, url, asset_digest(asset)?.as_ref()).await?;
            content.insert(
                RelPath::parse(&format!("mods/{filename}"))?,
                (blob.digest, blob.size),
            );
            if name == "lwjgl3ify" {
                let patch = asset["extra_assets"]
                    .as_array()
                    .context("missing lwjgl3ify extras")?
                    .iter()
                    .find(|a| {
                        a["filename"]
                            .as_str()
                            .is_some_and(|s| s.ends_with("-forgePatches.jar"))
                    })
                    .context("missing forgePatches asset")?;
                let blob = artifact_verified(
                    client,
                    store,
                    string(patch, "browser_download_url")?,
                    asset_digest(patch)?.as_ref(),
                )
                .await?;
                content.insert(
                    RelPath::parse("lwjgl3ify-forgePatches.jar")?,
                    (blob.digest, blob.size),
                );
            }
        }
    }
    Ok(())
}

fn wanted(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    !first.starts_with('.')
        && !first.eq_ignore_ascii_case("world")
        && !first.to_ascii_lowercase().starts_with("world_")
        && !["logs", "backups", "crash-reports"].contains(&first)
        && !hopper_core::plan::reconcile::PROTECTED_FILES.contains(&path)
        && !["eula.txt", "server.properties", "jvm.args"].contains(&path)
        && !path.ends_with(".sh")
        && !path.ends_with(".bat")
        && !path.ends_with(".ps1")
}

/// Validate *every* ZIP entry, including entries later excluded from installation.
pub fn archive(bytes: &[u8], store: &BlobStore) -> Result<BTreeMap<RelPath, (Digest, u64)>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    ensure!(
        zip.len() <= mrpack::MAX_ENTRIES,
        "GTNH archive has too many entries"
    );
    let names: Vec<String> = zip
        .file_names()
        .filter(|n| !n.ends_with('/'))
        .map(str::to_owned)
        .collect();
    let prefix = names
        .first()
        .and_then(|n| n.split_once('/'))
        .filter(|(p, _)| {
            ![
                "config",
                "mods",
                "libraries",
                "scripts",
                "resources",
                "world",
                "logs",
                "backups",
                "crash-reports",
            ]
            .contains(&p.to_ascii_lowercase().as_str())
                && !p.starts_with('.')
                && !p.to_ascii_lowercase().starts_with("world_")
        })
        .map(|(p, _)| format!("{p}/"))
        .filter(|p| names.iter().all(|n| n.starts_with(p)));
    let mut content = BTreeMap::new();
    let mut total = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        ensure!(
            !entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000),
            "symlink in GTNH archive"
        );
        RelPath::parse(entry.name().trim_end_matches('/'))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .strip_prefix(prefix.as_deref().unwrap_or(""))
            .unwrap_or(entry.name())
            .to_owned();
        let path = RelPath::parse(&name)?;
        ensure!(
            seen.insert(path.clone()),
            "duplicate GTNH archive path {path}"
        );
        ensure!(
            entry.size() <= mrpack::MAX_ENTRY_BYTES,
            "GTNH archive entry too large"
        );
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(mrpack::MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut data)?;
        total = total
            .checked_add(data.len() as u64)
            .context("archive size overflow")?;
        ensure!(
            data.len() as u64 <= mrpack::MAX_ENTRY_BYTES && total <= mrpack::MAX_TOTAL_BYTES,
            "GTNH archive decompression limit exceeded"
        );
        if wanted(&name) {
            let blob = store.insert_bytes(&data, None)?;
            content.insert(path, (blob.digest, blob.size));
        }
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_lts_selection() {
        assert_eq!(java_major(26, None).unwrap(), 25);
        assert_eq!(java_major(21, None).unwrap(), 21);
        assert_eq!(java_major(20, None).unwrap(), 17);
        assert!(java_major(21, Some(25)).is_err());
    }
    #[test]
    fn testing_is_not_stable_and_rc_wins_over_beta_on_same_date() {
        let release = |title: &str| Release {
            title: title.into(),
            date: "2026-09-01".into(),
            max_java: 25,
            server: ServerUrls {
                legacy: None,
                modern: Some("https://downloads.gtnewhorizons.com/server.zip".into()),
            },
        };
        let releases = BTreeMap::from([
            ("2.8.4".into(), release("Stable release")),
            ("2.9.0-beta-3".into(), release("Beta release")),
            ("2.9.0-RC-1".into(), release("Beta release")),
        ]);
        assert_eq!(choose(&releases, "stable", None).unwrap().0, "2.8.4");
        assert_eq!(choose(&releases, "testing", None).unwrap().0, "2.9.0-RC-1");
        assert!(
            choose(
                &BTreeMap::from([("2.8.4".into(), release("Stable release"))]),
                "testing",
                None
            )
            .is_err()
        );
    }
    #[test]
    fn published_asset_checksums_are_validated() {
        let hash = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            asset_digest(&serde_json::json!({"digest":hash}))
                .unwrap()
                .unwrap()
                .to_string(),
            hash
        );
        assert!(asset_digest(&serde_json::json!({"digest":"sha256:bad"})).is_err());
        assert!(
            asset_digest(&serde_json::json!({"digest":null}))
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn cached_artifacts_check_published_sha256_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path());
        let blob = store.insert_bytes(b"verified artifact", None).unwrap();
        let url = "https://downloads.gtnewhorizons.com/exact-version.zip";
        let mut hasher = hopper_core::MultiHasher::new();
        hasher.update(url.as_bytes());
        let directory = store.root().join("gtnh-urls");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(hasher.finish().1.to_string().replace(':', "-")),
            serde_json::to_vec(&(url, &blob.digest)).unwrap(),
        )
        .unwrap();
        let mut expected = hopper_core::MultiHasher::new().with_sha256();
        expected.update(b"verified artifact");
        let hash = expected.finish().0.sha256.unwrap();
        let client = HttpClient::new(&crate::user_agent(), HostAllowlist::gtnh()).unwrap();
        assert_eq!(
            artifact_verified(&client, &store, url, Some(&hash))
                .await
                .unwrap()
                .digest,
            blob.digest
        );
        let wrong = Digest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
        assert!(
            artifact_verified(&client, &store, url, Some(&wrong))
                .await
                .is_err()
        );
    }
    #[test]
    fn authoritative_sides_and_protected_files() {
        assert!(server_side("SERVER_JAVA9").unwrap());
        assert!(!server_side("CLIENT").unwrap());
        assert!(server_side("NEW_SIDE").is_err());
        for path in ["World/level.dat", "ops.json", "eula.txt", "startserver.sh"] {
            assert!(!wanted(path));
        }
        assert!(wanted("libraries/forge.jar"));
    }
    #[test]
    fn rejects_archive_traversal() {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("../ops.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"[]").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let temp = tempfile::tempdir().unwrap();
        assert!(archive(&bytes, &BlobStore::new(temp.path())).is_err());
    }
    #[test]
    fn config_only_archive_keeps_its_directory() {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "config/edited.cfg",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(b"configuration").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let temp = tempfile::tempdir().unwrap();
        let entries = archive(&bytes, &BlobStore::new(temp.path())).unwrap();
        assert!(entries.contains_key(&RelPath::parse("config/edited.cfg").unwrap()));
        assert!(!entries.contains_key(&RelPath::parse("edited.cfg").unwrap()));
    }
}
