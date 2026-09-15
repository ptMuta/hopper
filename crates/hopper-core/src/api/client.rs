//! Resolving a Modrinth slug into a downloadable pack.
//!
//! Version selection is the interesting part. A project lists many versions and the newest is
//! not always the right one: it may target a Minecraft version the operator did not ask for,
//! and it may be an alpha. So the rule is "newest release that satisfies the constraints",
//! falling back through beta and alpha only if nothing stable matches — and saying so.

use crate::api::modrinth::{API_V2, VersionType, WireProject, WireVersion};
use crate::model::MinecraftVersion;
use crate::net::{HttpClient, HttpError};

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("{slug:?} is a {kind}, not a modpack")]
    NotAModpack { slug: String, kind: String },
    #[error("{slug:?} was not found on Modrinth")]
    NotFound { slug: String },
    #[error("{slug:?} has no versions")]
    NoVersions { slug: String },
    #[error("{slug:?} has no version {version:?}")]
    NoSuchVersion { slug: String, version: String },
    #[error("{slug:?} has no version for Minecraft {mc}")]
    NoVersionForMinecraft { slug: String, mc: MinecraftVersion },
    #[error("version {version:?} of {slug:?} has no downloadable file")]
    NoFile { slug: String, version: String },
}

/// A resolved pack version, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackVersion {
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub version_type: Option<VersionType>,
    pub file_url: String,
    pub file_name: String,
    pub size: u64,
    pub hashes: crate::model::Hashes,
}

pub async fn fetch_project(client: &HttpClient, slug: &str) -> Result<WireProject, RegistryError> {
    let url = format!("{API_V2}/project/{slug}");
    match client.get_json::<WireProject>(&url).await {
        Ok(p) => Ok(p),
        Err(HttpError::Status { status: 404, .. }) => Err(RegistryError::NotFound {
            slug: slug.to_owned(),
        }),
        Err(e) => Err(e.into()),
    }
}

pub async fn fetch_versions(
    client: &HttpClient,
    project: &str,
) -> Result<Vec<WireVersion>, RegistryError> {
    // `include_changelog=false` because changelogs can be long and we never show them here.
    let url = format!("{API_V2}/project/{project}/version?include_changelog=false");
    Ok(client.get_json(&url).await?)
}

/// Pick the version to install.
///
/// Prefers releases, then betas, then alphas, so a pack that has only ever shipped betas is
/// still installable rather than silently unavailable.
pub fn choose_version(
    versions: &[WireVersion],
    slug: &str,
    wanted: Option<&str>,
    mc: Option<&MinecraftVersion>,
) -> Result<WireVersion, RegistryError> {
    if versions.is_empty() {
        return Err(RegistryError::NoVersions {
            slug: slug.to_owned(),
        });
    }

    if let Some(want) = wanted {
        return versions
            .iter()
            .find(|v| v.version_number == want || v.id.as_str() == want)
            .cloned()
            .ok_or_else(|| RegistryError::NoSuchVersion {
                slug: slug.to_owned(),
                version: want.to_owned(),
            });
    }

    let matches_mc = |v: &WireVersion| match mc {
        Some(want) => v.game_versions.iter().any(|g| g == want.as_str()),
        None => true,
    };

    for kind in [VersionType::Release, VersionType::Beta, VersionType::Alpha] {
        if let Some(v) = versions
            .iter()
            .find(|v| v.version_type == Some(kind) && matches_mc(v))
        {
            return Ok(v.clone());
        }
    }
    // Some versions carry no type at all.
    if let Some(v) = versions.iter().find(|v| matches_mc(v)) {
        return Ok(v.clone());
    }

    Err(match mc {
        Some(mc) => RegistryError::NoVersionForMinecraft {
            slug: slug.to_owned(),
            mc: mc.clone(),
        },
        None => RegistryError::NoVersions {
            slug: slug.to_owned(),
        },
    })
}

/// Extract the `.mrpack` file from a chosen version.
pub fn pack_file(version: &WireVersion, slug: &str) -> Result<PackVersion, RegistryError> {
    // A modpack version's primary file is the .mrpack; prefer the extension when several
    // files are present, since sources jars and the like also appear.
    let file = version
        .files
        .iter()
        .find(|f| f.filename.ends_with(".mrpack"))
        .or_else(|| version.files.iter().find(|f| f.primary))
        .or_else(|| version.files.first())
        .ok_or_else(|| RegistryError::NoFile {
            slug: slug.to_owned(),
            version: version.version_number.clone(),
        })?;

    let mut hashes = crate::model::Hashes::default();
    for algo in [crate::model::HashAlgo::Sha1, crate::model::HashAlgo::Sha512] {
        if let Some(hex) = file.hashes.get(algo.name())
            && let Ok(d) = crate::model::Digest::new(algo, hex)
        {
            hashes.set(d);
        }
    }

    Ok(PackVersion {
        project_id: version.project_id.as_str().to_owned(),
        version_id: version.id.as_str().to_owned(),
        version_number: version.version_number.clone(),
        version_type: version.version_type,
        file_url: file.url.clone(),
        file_name: file.filename.clone(),
        size: file.size,
        hashes,
    })
}

/// Reject a project that is not a modpack, naming what it actually is.
pub fn require_modpack(project: &WireProject, slug: &str) -> Result<(), RegistryError> {
    if project.project_type == "modpack" {
        Ok(())
    } else {
        Err(RegistryError::NotAModpack {
            slug: slug.to_owned(),
            kind: project.project_type.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA512: &str = "ab00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f";

    fn version(
        number: &str,
        kind: Option<&str>,
        mc: &[&str],
        files: serde_json::Value,
    ) -> WireVersion {
        let mut v = serde_json::json!({
            "id": format!("id-{number}"),
            "project_id": "AANobbMI",
            "name": number,
            "version_number": number,
            "game_versions": mc,
            "loaders": ["fabric"],
            "files": files,
            "dependencies": [],
        });
        if let Some(k) = kind {
            v["version_type"] = serde_json::json!(k);
        }
        serde_json::from_value(v).unwrap()
    }

    fn mrpack_file() -> serde_json::Value {
        serde_json::json!([{
            "hashes": { "sha512": SHA512 },
            "url": "https://cdn.modrinth.com/data/X/versions/Y/pack.mrpack",
            "filename": "pack.mrpack",
            "primary": true,
            "size": 4096
        }])
    }

    #[test]
    fn prefers_the_newest_release() {
        let versions = vec![
            version("2.0.0-beta", Some("beta"), &["26.3"], mrpack_file()),
            version("1.9.0", Some("release"), &["26.3"], mrpack_file()),
            version("1.8.0", Some("release"), &["26.2"], mrpack_file()),
        ];
        let v = choose_version(&versions, "x", None, None).unwrap();
        assert_eq!(
            v.version_number, "1.9.0",
            "a beta must not win by being newer"
        );
    }

    #[test]
    fn falls_back_to_beta_then_alpha_rather_than_refusing() {
        let only_beta = vec![version(
            "0.1.0-beta",
            Some("beta"),
            &["26.3"],
            mrpack_file(),
        )];
        assert_eq!(
            choose_version(&only_beta, "x", None, None)
                .unwrap()
                .version_number,
            "0.1.0-beta"
        );

        let only_alpha = vec![version("0.0.1", Some("alpha"), &["26.3"], mrpack_file())];
        assert!(choose_version(&only_alpha, "x", None, None).is_ok());
    }

    #[test]
    fn honours_a_requested_minecraft_version() {
        let versions = vec![
            version("2.0.0", Some("release"), &["26.3"], mrpack_file()),
            version("1.0.0", Some("release"), &["1.21.1"], mrpack_file()),
        ];
        let v =
            choose_version(&versions, "x", None, Some(&MinecraftVersion::new("1.21.1"))).unwrap();
        assert_eq!(v.version_number, "1.0.0");
    }

    #[test]
    fn says_so_when_no_version_targets_the_requested_minecraft() {
        let versions = vec![version("2.0.0", Some("release"), &["26.3"], mrpack_file())];
        let err = choose_version(&versions, "x", None, Some(&MinecraftVersion::new("1.7.10")))
            .unwrap_err();
        assert!(
            matches!(err, RegistryError::NoVersionForMinecraft { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("1.7.10"));
    }

    #[test]
    fn an_explicit_version_can_be_given_by_number_or_id() {
        let versions = vec![
            version("1.0.0", Some("release"), &["26.3"], mrpack_file()),
            version("2.0.0", Some("release"), &["26.3"], mrpack_file()),
        ];
        assert_eq!(
            choose_version(&versions, "x", Some("1.0.0"), None)
                .unwrap()
                .version_number,
            "1.0.0"
        );
        assert_eq!(
            choose_version(&versions, "x", Some("id-2.0.0"), None)
                .unwrap()
                .version_number,
            "2.0.0"
        );
    }

    #[test]
    fn a_missing_requested_version_is_named() {
        let versions = vec![version("1.0.0", Some("release"), &["26.3"], mrpack_file())];
        let err = choose_version(&versions, "x", Some("9.9.9"), None).unwrap_err();
        assert!(matches!(err, RegistryError::NoSuchVersion { .. }));
        assert!(err.to_string().contains("9.9.9"));
    }

    #[test]
    fn an_empty_project_is_reported_rather_than_panicking() {
        assert!(matches!(
            choose_version(&[], "x", None, None).unwrap_err(),
            RegistryError::NoVersions { .. }
        ));
    }

    #[test]
    fn versions_without_a_type_are_still_usable() {
        let versions = vec![version("1.0.0", None, &["26.3"], mrpack_file())];
        assert!(choose_version(&versions, "x", None, None).is_ok());
    }

    #[test]
    fn the_mrpack_is_picked_out_of_several_files() {
        // Some versions attach extra files; the extension is the reliable discriminator.
        let v = version(
            "1.0.0",
            Some("release"),
            &["26.3"],
            serde_json::json!([
                { "hashes": {"sha512": SHA512}, "url": "https://cdn.modrinth.com/a.jar",
                  "filename": "extra.jar", "primary": true, "size": 1 },
                { "hashes": {"sha512": SHA512}, "url": "https://cdn.modrinth.com/p.mrpack",
                  "filename": "pack.mrpack", "primary": false, "size": 2 },
            ]),
        );
        let p = pack_file(&v, "x").unwrap();
        assert_eq!(p.file_name, "pack.mrpack");
    }

    #[test]
    fn hashes_are_carried_through_for_verification() {
        let v = version("1.0.0", Some("release"), &["26.3"], mrpack_file());
        let p = pack_file(&v, "x").unwrap();
        assert!(p.hashes.get(crate::model::HashAlgo::Sha512).is_some());
        assert_eq!(p.size, 4096);
    }

    #[test]
    fn a_version_with_no_files_is_an_error() {
        let v = version("1.0.0", Some("release"), &["26.3"], serde_json::json!([]));
        assert!(matches!(
            pack_file(&v, "x").unwrap_err(),
            RegistryError::NoFile { .. }
        ));
    }

    #[test]
    fn a_mod_project_is_refused_by_name() {
        let p: WireProject = serde_json::from_value(serde_json::json!({
            "id": "AANobbMI", "slug": "sodium", "title": "Sodium", "project_type": "mod",
        }))
        .unwrap();
        let err = require_modpack(&p, "sodium").unwrap_err();
        assert!(
            err.to_string().contains("is a mod, not a modpack"),
            "got {err}"
        );
    }

    #[test]
    fn a_modpack_project_is_accepted() {
        let p: WireProject = serde_json::from_value(serde_json::json!({
            "id": "X", "title": "P", "project_type": "modpack",
        }))
        .unwrap();
        assert!(require_modpack(&p, "p").is_ok());
    }
}
