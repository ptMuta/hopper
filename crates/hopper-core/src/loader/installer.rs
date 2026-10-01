//! Forge and NeoForge: what their installer jar produces, and what we keep of it.
//!
//! Neither publishes a launch descriptor the way Fabric does. The installer patches the vanilla
//! jar and lays down a `libraries/` tree, so the only way to learn the result is to run it.
//! Running it happens in a cache staging directory, never in the server directory; this module
//! decides which of the staged files become managed files and how the server is launched.
//!
//! The installer's own `run.sh`, `run.bat` and `user_jvm_args.txt` are dropped deliberately.
//! `start.sh` and `jvm.args` replace them, so there is one knob to learn whatever the loader.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::loader::LaunchProfile;
use crate::model::{Digest, HashAlgo, LoaderKind, MinecraftVersion, RelPath};

#[derive(Debug, thiserror::Error)]
pub enum InstallerError {
    #[error("{0} is not installed by running an installer jar")]
    NotAnInstallerLoader(LoaderKind),
    #[error(
        "the {loader} installer finished but produced no way to start the server \
         (no unix_args.txt and no runnable forge jar)"
    )]
    NoLaunchTarget { loader: LoaderKind },
    #[error("the installer produced an unusable path {0:?}")]
    BadPath(String),
    #[error("malformed checksum sidecar: {0:?}")]
    BadSidecar(String),
    #[error("reading installer output: {0}")]
    Io(#[from] std::io::Error),
}

/// Maven coordinates of the installer for a resolved loader build.
pub fn installer_url(
    loader: LoaderKind,
    mc: &MinecraftVersion,
    version: &str,
) -> Result<String, InstallerError> {
    match loader {
        LoaderKind::Forge => Ok(super::ForgePromotions::installer_url(mc, version)),
        // NeoForge's first release, for 1.20.1, still used Forge's artifact layout under the
        // NeoForged group. Every later Minecraft version uses `net.neoforged:neoforge`.
        LoaderKind::NeoForge if mc.as_str() == "1.20.1" => {
            let v = format!("1.20.1-{version}");
            Ok(format!(
                "https://maven.neoforged.net/releases/net/neoforged/forge/{v}/forge-{v}-installer.jar"
            ))
        }
        LoaderKind::NeoForge => Ok(format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar"
        )),
        other => Err(InstallerError::NotAnInstallerLoader(other)),
    }
}

/// The SHA-1 a Maven repository publishes beside an artifact.
pub fn sidecar_url(artifact_url: &str) -> String {
    format!("{artifact_url}.sha1")
}

/// A `.sha1` sidecar's digest. Some repositories append the file name after the hex.
pub fn parse_sha1_sidecar(body: &str) -> Result<Digest, InstallerError> {
    let hex = body.split_whitespace().next().unwrap_or_default();
    Digest::new(HashAlgo::Sha1, hex).map_err(|_| InstallerError::BadSidecar(body.to_owned()))
}

/// The newest stable NeoForge build in a series, falling back to the newest of any kind.
///
/// `candidates` is newest first, as [`super::neoforge_versions_for`] returns it.
pub fn choose_neoforge(candidates: &[String]) -> Option<&str> {
    candidates
        .iter()
        .find(|v| !v.contains('-'))
        .or_else(|| candidates.first())
        .map(String::as_str)
}

/// Where a build's installer output is staged, relative to the cache root.
///
/// Keyed by everything that changes the output, so two servers on the same build share one
/// installer run.
pub fn staging_dir(
    cache_root: &Path,
    loader: LoaderKind,
    mc: &MinecraftVersion,
    version: &str,
) -> PathBuf {
    cache_root.join("installers").join(format!(
        "{}-{}-{}",
        loader.mrpack_key().unwrap_or("vanilla"),
        mc.as_str(),
        version
    ))
}

/// Recorded once an installer run has been collected, so later runs skip both the installer and
/// re-hashing its output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub files: Vec<RecordedFile>,
    pub launch: LaunchProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedFile {
    pub path: RelPath,
    pub content: Digest,
    pub size: u64,
}

impl InstallRecord {
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("an install record always serializes")
    }
}

/// The record's file name inside a staging directory. Its presence marks a complete run.
pub const RECORD_FILE: &str = "hopper-install.json";

/// A staged file worth keeping, with where it is on disk now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptFile {
    pub path: RelPath,
    pub source: PathBuf,
}

/// Choose what to keep from an installer's output and how to launch the result.
pub fn collect(
    staging: &Path,
    loader: LoaderKind,
) -> Result<(Vec<KeptFile>, LaunchProfile), InstallerError> {
    if !matches!(loader, LoaderKind::Forge | LoaderKind::NeoForge) {
        return Err(InstallerError::NotAnInstallerLoader(loader));
    }

    let mut kept = Vec::new();
    walk(staging, staging, &mut kept)?;
    kept.retain(|f| keep(&f.path));
    kept.sort_by(|a, b| a.path.cmp(&b.path));

    let launch = launch_for(&kept, loader)?;
    Ok((kept, launch))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<KeptFile>) -> Result<(), InstallerError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        // The installer has no business creating links, and following one could pull in files
        // from anywhere on the machine.
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("walked from root")
            .to_string_lossy()
            .replace('\\', "/");
        let rel = RelPath::parse(&rel).map_err(|_| InstallerError::BadPath(rel.clone()))?;
        out.push(KeptFile {
            path: rel,
            source: path,
        });
    }
    Ok(())
}

/// Which installer outputs become managed files.
fn keep(path: &RelPath) -> bool {
    if path.starts_with_dir("libraries") {
        return true;
    }
    // Only the root level matters beyond libraries/: the legacy runnable jar and the vanilla jar
    // it expects beside it. Scripts, logs and the operator-facing args file are ours to replace.
    if path.as_str().contains('/') {
        return false;
    }
    let name = path.as_str();
    name.ends_with(".jar") && !name.ends_with("-installer.jar")
}

fn launch_for(kept: &[KeptFile], loader: LoaderKind) -> Result<LaunchProfile, InstallerError> {
    let argfile = |name: &str| {
        kept.iter()
            .map(|f| &f.path)
            .find(|p| {
                p.file_name() == name
                    && (p.starts_with_dir("libraries/net/minecraftforge/forge")
                        || p.starts_with_dir("libraries/net/neoforged"))
            })
            .cloned()
    };

    // Forge 1.17+ and every NeoForge build.
    if let Some(unix) = argfile("unix_args.txt") {
        return Ok(LaunchProfile::ArgFiles {
            module_argfile_unix: unix,
            module_argfile_windows: argfile("win_args.txt"),
            program_args: vec!["nogui".into()],
        });
    }

    // Older Forge: a runnable jar at the root, named after the build.
    let jar = kept
        .iter()
        .map(|f| &f.path)
        .find(|p| {
            let n = p.as_str();
            !n.contains('/')
                && n.starts_with("forge-")
                && n.ends_with(".jar")
                && !n.ends_with("-installer.jar")
        })
        .cloned();
    match jar {
        Some(jar) => Ok(LaunchProfile::ExecutableJar {
            jar,
            jvm_args: vec![],
            program_args: vec!["nogui".into()],
        }),
        None => Err(InstallerError::NoLaunchTarget { loader }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mc(s: &str) -> MinecraftVersion {
        MinecraftVersion::new(s)
    }

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, f.as_bytes()).unwrap();
        }
        dir
    }

    fn paths(kept: &[KeptFile]) -> Vec<&str> {
        kept.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn modern_forge_launches_from_its_argfile_and_drops_its_scripts() {
        let dir = tree(&[
            "libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt",
            "libraries/net/minecraftforge/forge/1.20.1-47.2.0/win_args.txt",
            "libraries/net/minecraft/server/1.20.1/server-1.20.1.jar",
            "run.sh",
            "run.bat",
            "user_jvm_args.txt",
            "forge-1.20.1-47.2.0-installer.jar.log",
        ]);
        let (kept, launch) = collect(dir.path(), LoaderKind::Forge).unwrap();

        assert_eq!(
            paths(&kept),
            [
                "libraries/net/minecraft/server/1.20.1/server-1.20.1.jar",
                "libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt",
                "libraries/net/minecraftforge/forge/1.20.1-47.2.0/win_args.txt",
            ]
        );
        assert_eq!(
            launch,
            LaunchProfile::ArgFiles {
                module_argfile_unix: RelPath::parse(
                    "libraries/net/minecraftforge/forge/1.20.1-47.2.0/unix_args.txt"
                )
                .unwrap(),
                module_argfile_windows: Some(
                    RelPath::parse("libraries/net/minecraftforge/forge/1.20.1-47.2.0/win_args.txt")
                        .unwrap()
                ),
                program_args: vec!["nogui".into()],
            }
        );
    }

    #[test]
    fn neoforge_launches_from_its_argfile() {
        let dir = tree(&["libraries/net/neoforged/neoforge/21.1.95/unix_args.txt"]);
        let (_, launch) = collect(dir.path(), LoaderKind::NeoForge).unwrap();
        assert!(matches!(launch, LaunchProfile::ArgFiles { .. }));
    }

    #[test]
    fn legacy_forge_launches_its_runnable_jar_and_keeps_the_vanilla_jar() {
        let dir = tree(&[
            "forge-1.16.5-36.2.39.jar",
            "minecraft_server.1.16.5.jar",
            "forge-1.16.5-36.2.39-installer.jar",
            "libraries/com/example/lib/1.0/lib-1.0.jar",
        ]);
        let (kept, launch) = collect(dir.path(), LoaderKind::Forge).unwrap();

        assert_eq!(
            paths(&kept),
            [
                "forge-1.16.5-36.2.39.jar",
                "libraries/com/example/lib/1.0/lib-1.0.jar",
                "minecraft_server.1.16.5.jar",
            ]
        );
        assert_eq!(
            launch,
            LaunchProfile::ExecutableJar {
                jar: RelPath::parse("forge-1.16.5-36.2.39.jar").unwrap(),
                jvm_args: vec![],
                program_args: vec!["nogui".into()],
            }
        );
    }

    #[test]
    fn output_with_nothing_to_launch_is_an_error() {
        let dir = tree(&["libraries/com/example/lib/1.0/lib-1.0.jar", "run.sh"]);
        assert!(matches!(
            collect(dir.path(), LoaderKind::Forge),
            Err(InstallerError::NoLaunchTarget { .. })
        ));
    }

    #[test]
    fn fabric_is_not_an_installer_loader() {
        let dir = tree(&[]);
        assert!(collect(dir.path(), LoaderKind::Fabric).is_err());
        assert!(installer_url(LoaderKind::Fabric, &mc("1.21.1"), "0.16.0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_the_output_are_ignored() {
        let dir = tree(&["libraries/net/neoforged/neoforge/21.1.95/unix_args.txt"]);
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("libraries/passwd")).unwrap();
        let (kept, _) = collect(dir.path(), LoaderKind::NeoForge).unwrap();
        assert!(kept.iter().all(|f| f.path.as_str() != "libraries/passwd"));
    }

    #[test]
    fn installer_urls_follow_each_maven_layout() {
        assert_eq!(
            installer_url(LoaderKind::NeoForge, &mc("1.21.1"), "21.1.95").unwrap(),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.95/neoforge-21.1.95-installer.jar"
        );
        assert_eq!(
            installer_url(LoaderKind::NeoForge, &mc("1.20.1"), "47.1.106").unwrap(),
            "https://maven.neoforged.net/releases/net/neoforged/forge/1.20.1-47.1.106/forge-1.20.1-47.1.106-installer.jar"
        );
        assert_eq!(
            installer_url(LoaderKind::Forge, &mc("1.20.1"), "47.2.0").unwrap(),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar"
        );
    }

    #[test]
    fn sidecars_may_carry_a_file_name_after_the_hex() {
        let hex = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        assert_eq!(parse_sha1_sidecar(hex).unwrap().hex(), hex);
        assert_eq!(
            parse_sha1_sidecar(&format!("{hex}  forge-installer.jar\n"))
                .unwrap()
                .hex(),
            hex
        );
        assert!(parse_sha1_sidecar("<html>not found</html>").is_err());
    }

    #[test]
    fn neoforge_prefers_a_stable_build() {
        let v = vec!["21.1.96-beta".to_owned(), "21.1.95".to_owned()];
        assert_eq!(choose_neoforge(&v), Some("21.1.95"));
        let only_beta = vec!["21.1.96-beta".to_owned()];
        assert_eq!(choose_neoforge(&only_beta), Some("21.1.96-beta"));
        assert_eq!(choose_neoforge(&[]), None);
    }

    #[test]
    fn staging_is_keyed_by_loader_minecraft_and_build() {
        let a = staging_dir(Path::new("/c"), LoaderKind::Forge, &mc("1.20.1"), "47.2.0");
        let b = staging_dir(
            Path::new("/c"),
            LoaderKind::NeoForge,
            &mc("1.20.1"),
            "47.2.0",
        );
        assert_ne!(a, b);
        assert!(a.starts_with("/c/installers"));
    }
}
