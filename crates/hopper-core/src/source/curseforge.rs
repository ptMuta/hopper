//! Reading CurseForge pack archives: the client pack's manifest and overrides, and the server
//! pack an author may publish beside it.
//!
//! A server pack has no index. It is a server directory zipped up, in whatever shape its
//! author's tooling produced: sometimes wrapped in a top-level folder, usually carrying a
//! loader installer and launch scripts, occasionally a prebuilt `libraries/` tree. hopper keeps
//! the content and drops everything it provides itself, so a server pack installs exactly like
//! any other pack: as managed files the next update can reconcile.

use std::io::{Read, Seek};

use crate::api::curseforge::{CurseForgeError, Manifest};
use crate::model::{LoaderKind, RelPath};
use crate::source::mrpack::{
    MAX_ENTRIES, MAX_INDEX_BYTES, MrpackError, OverrideEntry, OverrideKind, check_entry,
};

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error(transparent)]
    Archive(#[from] MrpackError),
    #[error("archive has no manifest.json at its root, so it is not a CurseForge modpack")]
    MissingManifest,
    #[error(transparent)]
    Manifest(#[from] CurseForgeError),
}

/// A client pack: what to resolve, and the overrides to lay over it.
#[derive(Debug, Clone)]
pub struct ClientPack {
    pub manifest: Manifest,
    pub overrides: Vec<OverrideEntry>,
}

/// A server pack, reduced to the files hopper should manage.
#[derive(Debug, Clone)]
pub struct ServerPack {
    pub entries: Vec<OverrideEntry>,
    /// Entries left out because hopper provides them itself, for reporting.
    pub dropped: Vec<String>,
    /// The loader build the author bundled an installer for, if any.
    pub bundled_loader: Option<(LoaderKind, String)>,
}

pub fn read_client_pack<R: Read + Seek>(reader: R) -> Result<ClientPack, PackError> {
    let mut zip = open(reader)?;

    let manifest = {
        let mut entry = zip
            .by_name("manifest.json")
            .map_err(|_| PackError::MissingManifest)?;
        let mut buf = Vec::new();
        entry
            .by_ref()
            .take(MAX_INDEX_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| MrpackError::Io(e.to_string()))?;
        if buf.len() as u64 > MAX_INDEX_BYTES {
            return Err(MrpackError::EntryTooLarge {
                path: "manifest.json".into(),
                limit: MAX_INDEX_BYTES,
            }
            .into());
        }
        Manifest::parse(&buf)?
    };

    let prefix = format!("{}/", manifest.overrides);
    let mut overrides = Vec::new();
    let mut total = 0;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| MrpackError::Archive(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let size = check_entry(&entry, &mut total)?;
        overrides.push(OverrideEntry {
            path: parse_path(&name, rest)?,
            kind: OverrideKind::Global,
            zip_name: name,
            size,
        });
    }
    overrides.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ClientPack {
        manifest,
        overrides,
    })
}

pub fn read_server_pack<R: Read + Seek>(reader: R) -> Result<ServerPack, PackError> {
    let mut zip = open(reader)?;
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    let wrapper = wrapper_dir(&names);

    let mut entries = Vec::new();
    let mut dropped = Vec::new();
    let mut bundled_loader = None;
    let mut total = 0;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| MrpackError::Archive(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        if is_debris(&name) {
            continue;
        }
        let rest = match &wrapper {
            Some(w) => name.strip_prefix(w.as_str()).unwrap_or(&name),
            None => &name,
        };
        if rest.is_empty() {
            continue;
        }
        if let Some(found) = installer_build(rest) {
            bundled_loader = Some(found);
        }
        // Checked before deciding to drop, so a symlink is refused wherever it sits rather
        // than only where we happen to look.
        let size = check_entry(&entry, &mut total)?;
        if provided_by_hopper(rest) {
            dropped.push(rest.to_owned());
            continue;
        }
        entries.push(OverrideEntry {
            path: parse_path(&name, rest)?,
            kind: OverrideKind::Server,
            zip_name: name,
            size,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    dropped.sort();
    Ok(ServerPack {
        entries,
        dropped,
        bundled_loader,
    })
}

fn open<R: Read + Seek>(reader: R) -> Result<zip::ZipArchive<R>, MrpackError> {
    let zip = zip::ZipArchive::new(reader).map_err(|e| MrpackError::Archive(e.to_string()))?;
    if zip.len() > MAX_ENTRIES {
        return Err(MrpackError::TooManyEntries(zip.len()));
    }
    Ok(zip)
}

fn parse_path(zip_name: &str, rest: &str) -> Result<RelPath, MrpackError> {
    RelPath::parse(rest).map_err(|source| MrpackError::BadPath {
        path: zip_name.to_owned(),
        source,
    })
}

/// Folder names that are server content in their own right, never a wrapper.
const CONTENT_DIRS: &[&str] = &[
    "mods",
    "config",
    "defaultconfigs",
    "kubejs",
    "scripts",
    "resourcepacks",
    "datapacks",
    "libraries",
    "plugins",
];

/// Archive debris that is never server content: macOS resource forks and the like.
fn is_debris(name: &str) -> bool {
    name.starts_with("__MACOSX/") || name.ends_with("/.DS_Store") || name == ".DS_Store"
}

/// The single folder a server pack was zipped from, if it was zipped that way.
///
/// Stripped when every entry is inside one folder that is not itself a content folder. A pack
/// that ships nothing but `config/` keeps that folder name.
fn wrapper_dir(names: &[String]) -> Option<String> {
    let mut real = names.iter().filter(|n| !is_debris(n));
    let first = real.next()?.split_once('/')?.0.to_owned();
    // `..` or `.` is not a folder anyone zipped from; stripping it would quietly turn a hostile
    // path into an innocent one instead of refusing it.
    if CONTENT_DIRS.contains(&first.as_str()) || RelPath::parse(&first).is_err() {
        return None;
    }
    let prefix = format!("{first}/");
    names
        .iter()
        .filter(|n| !is_debris(n))
        .all(|n| n.starts_with(&prefix))
        .then_some(prefix)
}

/// Whether a server-pack entry is something hopper provides or must never write.
fn provided_by_hopper(path: &str) -> bool {
    // Trees owned by the loader installer, or by the running server.
    for dir in [
        "libraries/",
        "world/",
        "logs/",
        "crash-reports/",
        ".fabric/",
    ] {
        if path.starts_with(dir) {
            return true;
        }
    }
    if path.contains('/') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    let script = [".sh", ".bat", ".cmd", ".ps1", ".command"]
        .iter()
        .any(|ext| lower.ends_with(ext));
    // Launch scripts and the args file are replaced by start.sh and jvm.args.
    (script && (lower.starts_with("run") || lower.starts_with("start") || lower.contains("server")))
        || matches!(
            lower.as_str(),
            "user_jvm_args.txt" | "eula.txt" | "server.properties" | "server.jar" | "variables.txt"
        )
        // The installer and anything it would have produced; hopper runs its own.
        || lower.ends_with("-installer.jar")
        || lower.ends_with(".log")
        || (lower.ends_with(".jar") && (lower.starts_with("forge-") || lower.starts_with("minecraft_server")))
}

/// `forge-1.20.1-47.4.0-installer.jar` -> Forge 47.4.0.
fn installer_build(path: &str) -> Option<(LoaderKind, String)> {
    if path.contains('/') {
        return None;
    }
    let stem = path.strip_suffix("-installer.jar")?;
    if let Some(v) = stem.strip_prefix("neoforge-") {
        return Some((LoaderKind::NeoForge, v.to_owned()));
    }
    let rest = stem.strip_prefix("forge-")?;
    // Forge spells its builds `<mc>-<build>`; the build is what the installer URL needs.
    let (_, build) = rest.split_once('-')?;
    Some((LoaderKind::Forge, build.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn build_zip(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        let mut c = w.finish().unwrap();
        c.set_position(0);
        c
    }

    fn kept(sp: &ServerPack) -> Vec<&str> {
        sp.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn a_deceasedcraft_shaped_server_pack_keeps_content_and_drops_what_hopper_provides() {
        // The shape of DeceasedCraft's real server pack, read from its central directory.
        let sp = read_server_pack(build_zip(&[
            ("mods/create.jar", b"m"),
            ("config/a.toml", b"c"),
            ("defaultconfigs/forge-server.toml", b"d"),
            ("kubejs/server_scripts/x.js", b"k"),
            ("tacz/tacz-pre.toml", b"t"),
            ("default-server.properties", b"p"),
            ("forge-1.20.1-47.4.0-installer.jar", b"i"),
            ("run.bat", b"r"),
            ("user_jvm_args.txt", b"u"),
        ]))
        .unwrap();

        assert_eq!(
            kept(&sp),
            [
                "config/a.toml",
                "default-server.properties",
                "defaultconfigs/forge-server.toml",
                "kubejs/server_scripts/x.js",
                "mods/create.jar",
                "tacz/tacz-pre.toml",
            ]
        );
        assert_eq!(
            sp.dropped,
            [
                "forge-1.20.1-47.4.0-installer.jar",
                "run.bat",
                "user_jvm_args.txt"
            ]
        );
        assert_eq!(
            sp.bundled_loader,
            Some((LoaderKind::Forge, "47.4.0".into()))
        );
        assert!(sp.entries.iter().all(|e| e.kind == OverrideKind::Server));
    }

    #[test]
    fn a_wrapper_folder_is_stripped() {
        let sp = read_server_pack(build_zip(&[
            ("Pack Server 1.0/mods/a.jar", b"a"),
            ("Pack Server 1.0/config/b.toml", b"b"),
            ("Pack Server 1.0/startserver.sh", b"s"),
        ]))
        .unwrap();
        assert_eq!(kept(&sp), ["config/b.toml", "mods/a.jar"]);
        assert_eq!(sp.dropped, ["startserver.sh"]);
        // The zip name still points at the real entry for extraction.
        assert!(sp.entries[0].zip_name.starts_with("Pack Server 1.0/"));
    }

    #[test]
    fn a_wrapper_is_stripped_despite_macos_debris_and_without_mods() {
        let sp = read_server_pack(build_zip(&[
            ("Pack Server/config/a.toml", b"a"),
            ("Pack Server/kubejs/b.js", b"b"),
            ("__MACOSX/Pack Server/._a.toml", b"x"),
        ]))
        .unwrap();
        assert_eq!(kept(&sp), ["config/a.toml", "kubejs/b.js"]);
    }

    #[test]
    fn a_lone_content_folder_is_not_mistaken_for_a_wrapper() {
        let sp = read_server_pack(build_zip(&[
            ("config/a.toml", b"a"),
            ("config/b.toml", b"b"),
        ]))
        .unwrap();
        assert_eq!(kept(&sp), ["config/a.toml", "config/b.toml"]);
    }

    #[test]
    fn prebuilt_loader_output_and_server_state_are_dropped() {
        let sp = read_server_pack(build_zip(&[
            ("mods/a.jar", b"a"),
            ("libraries/net/minecraftforge/forge/x/unix_args.txt", b"l"),
            ("world/level.dat", b"w"),
            ("server.properties", b"p"),
            ("eula.txt", b"e"),
            ("forge-1.16.5-36.2.39.jar", b"f"),
            ("minecraft_server.1.16.5.jar", b"v"),
            ("logs/latest.log", b"x"),
        ]))
        .unwrap();
        assert_eq!(kept(&sp), ["mods/a.jar"]);
    }

    #[test]
    fn traversal_in_a_server_pack_is_refused() {
        let err = read_server_pack(build_zip(&[("../escape.jar", b"x")])).unwrap_err();
        assert!(
            matches!(err, PackError::Archive(MrpackError::BadPath { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn symlinks_in_a_server_pack_are_refused_even_where_dropped() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.add_symlink(
            "run.sh",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        let mut c = w.finish().unwrap();
        c.set_position(0);
        assert!(matches!(
            read_server_pack(c),
            Err(PackError::Archive(MrpackError::SymlinkEntry(_)))
        ));
    }

    #[test]
    fn neoforge_installers_name_their_build() {
        assert_eq!(
            installer_build("neoforge-21.1.95-installer.jar"),
            Some((LoaderKind::NeoForge, "21.1.95".into()))
        );
        assert_eq!(
            installer_build("mods/forge-1.20.1-47.4.0-installer.jar"),
            None
        );
    }

    #[test]
    fn client_packs_yield_their_manifest_and_overrides() {
        let manifest = serde_json::json!({
            "minecraft": {"version": "1.20.1", "modLoaders": [{"id": "forge-47.4.0", "primary": true}]},
            "manifestType": "minecraftModpack",
            "files": [{"projectID": 1, "fileID": 2, "required": true}],
            "overrides": "overrides"
        })
        .to_string();
        let cp = read_client_pack(build_zip(&[
            ("manifest.json", manifest.as_bytes()),
            ("modlist.html", b"<html>"),
            ("overrides/config/a.toml", b"a"),
        ]))
        .unwrap();
        assert_eq!(cp.manifest.files.len(), 1);
        let paths: Vec<_> = cp.overrides.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, ["config/a.toml"]);
    }

    #[test]
    fn a_zip_without_a_manifest_is_not_a_client_pack() {
        assert!(matches!(
            read_client_pack(build_zip(&[("mods/a.jar", b"a")])),
            Err(PackError::MissingManifest)
        ));
    }
}
