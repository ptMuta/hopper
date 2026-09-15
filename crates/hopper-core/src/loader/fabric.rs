//! Fabric, via `meta.fabricmc.net`.
//!
//! Fabric is the one loader that needs no JVM to install. Its meta service publishes a launch
//! descriptor — main class plus the full library list with hashes — and Fabric never patches
//! the vanilla jar, so resolving that descriptor *is* the installation. Forge and NeoForge, by
//! contrast, run a bytecode-patching installer and must execute one.
//!
//! Note the hashes here are sha1 only. That is exactly why every file carries a separate
//! `verify` digest from upstream and a `content` sha512 we compute ourselves.

use serde::Deserialize;

use crate::model::{Digest, HashAlgo, MinecraftVersion, RelPath};

use super::launch::LaunchProfile;
use super::maven::{Coordinate, MavenError};

pub const META_BASE: &str = "https://meta.fabricmc.net";

/// Loader builds compatible with a Minecraft version, newest first.
pub fn loaders_url(mc: &MinecraftVersion) -> String {
    format!("{META_BASE}/v2/versions/loader/{}", mc.as_str())
}

/// The server launch descriptor for a specific loader build.
pub fn server_profile_url(mc: &MinecraftVersion, loader: &str) -> String {
    format!(
        "{META_BASE}/v2/versions/loader/{}/{loader}/server/json",
        mc.as_str()
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireLoaderEntry {
    pub loader: WireLoaderVersion,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireLoaderVersion {
    pub version: String,
    #[serde(default)]
    pub stable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireLibrary {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireServerProfile {
    pub id: String,
    pub main_class: String,
    #[serde(default)]
    pub libraries: Vec<WireLibrary>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FabricError {
    #[error("Fabric metadata is not valid JSON: {0}")]
    BadJson(String),
    #[error("Fabric metadata lists no loader for Minecraft {0}")]
    NoLoader(MinecraftVersion),
    #[error("library {name:?}: {source}")]
    BadLibrary {
        name: String,
        #[source]
        source: MavenError,
    },
    #[error("library {0:?} has no repository to download from")]
    NoRepository(String),
}

/// One library to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    pub coordinate: Coordinate,
    pub path: RelPath,
    pub url: String,
    /// sha1 when Fabric published one; some entries omit it.
    pub verify: Option<Digest>,
    pub size: Option<u64>,
}

/// Everything needed to run a Fabric server, resolved without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricServer {
    pub loader_version: String,
    pub libraries: Vec<Library>,
    pub launch: LaunchProfile,
}

/// Pick the newest stable loader build, falling back to the newest of any kind.
///
/// Fabric returns newest-first, so this is positional rather than a version comparison —
/// loader versions are not semver and must not be parsed.
pub fn choose_loader(bytes: &[u8], mc: &MinecraftVersion) -> Result<String, FabricError> {
    let entries: Vec<WireLoaderEntry> =
        serde_json::from_slice(bytes).map_err(|e| FabricError::BadJson(e.to_string()))?;
    entries
        .iter()
        .find(|e| e.loader.stable)
        .or_else(|| entries.first())
        .map(|e| e.loader.version.clone())
        .ok_or_else(|| FabricError::NoLoader(mc.clone()))
}

/// Parse the server launch descriptor into files plus a launch profile.
///
/// `server_jar` is the vanilla jar, which Fabric expects on the classpath but does not
/// distribute; it comes from Mojang.
pub fn parse_server_profile(
    bytes: &[u8],
    loader_version: &str,
    server_jar: &RelPath,
) -> Result<FabricServer, FabricError> {
    let wire: WireServerProfile =
        serde_json::from_slice(bytes).map_err(|e| FabricError::BadJson(e.to_string()))?;

    let mut libraries = Vec::with_capacity(wire.libraries.len());
    let mut classpath = Vec::with_capacity(wire.libraries.len() + 1);

    for lib in wire.libraries {
        let coordinate =
            Coordinate::parse(&lib.name).map_err(|source| FabricError::BadLibrary {
                name: lib.name.clone(),
                source,
            })?;
        let path = coordinate
            .install_path()
            .map_err(|source| FabricError::BadLibrary {
                name: lib.name.clone(),
                source,
            })?;
        let repo = lib
            .url
            .as_deref()
            .ok_or_else(|| FabricError::NoRepository(lib.name.clone()))?;
        let verify = lib
            .sha1
            .as_deref()
            .and_then(|hex| Digest::new(HashAlgo::Sha1, hex).ok());

        classpath.push(path.clone());
        libraries.push(Library {
            url: coordinate.url(repo),
            coordinate,
            path,
            verify,
            size: lib.size,
        });
    }

    // The vanilla jar goes last: loader classes must win when both define a class.
    classpath.push(server_jar.clone());

    Ok(FabricServer {
        loader_version: loader_version.to_owned(),
        libraries,
        launch: LaunchProfile::Classpath {
            main_class: wire.main_class,
            classpath,
            jvm_args: Vec::new(),
            program_args: vec!["nogui".into()],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1: &str = "aabbccddeeff00112233445566778899aabbccdd";

    fn profile_json() -> serde_json::Value {
        serde_json::json!({
            "id": "fabric-loader-0.17.2-26.3",
            "inheritsFrom": "26.3",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotServer",
            "libraries": [
                { "name": "net.fabricmc:fabric-loader:0.17.2",
                  "url": "https://maven.fabricmc.net/", "sha1": SHA1, "size": 1500000 },
                { "name": "net.fabricmc:intermediary:26.3",
                  "url": "https://maven.fabricmc.net/", "sha1": SHA1, "size": 900000 },
                { "name": "org.ow2.asm:asm:9.7",
                  "url": "https://repo1.maven.org/maven2/", "sha1": SHA1 }
            ]
        })
    }

    fn server_jar() -> RelPath {
        RelPath::parse("server.jar").unwrap()
    }

    #[test]
    fn resolves_a_server_profile_without_running_anything() {
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        assert_eq!(s.libraries.len(), 3);
        assert_eq!(s.loader_version, "0.17.2");
    }

    #[test]
    fn library_urls_come_from_each_entrys_own_repository() {
        // Fabric mixes its own maven with Maven Central; assuming one base breaks the other.
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        assert_eq!(
            s.libraries[0].url,
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar"
        );
        assert_eq!(
            s.libraries[2].url,
            "https://repo1.maven.org/maven2/org/ow2/asm/asm/9.7/asm-9.7.jar"
        );
    }

    #[test]
    fn libraries_install_under_the_libraries_directory() {
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        assert_eq!(
            s.libraries[0].path.as_str(),
            "libraries/net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar"
        );
    }

    #[test]
    fn the_vanilla_jar_goes_last_on_the_classpath() {
        // Loader classes must take precedence over the vanilla ones they replace.
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        let LaunchProfile::Classpath { classpath, .. } = &s.launch else {
            panic!("fabric should produce a classpath launch");
        };
        assert_eq!(classpath.len(), 4);
        assert_eq!(classpath.last().unwrap().as_str(), "server.jar");
    }

    #[test]
    fn the_main_class_comes_from_the_metadata_not_a_constant() {
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        let LaunchProfile::Classpath { main_class, .. } = &s.launch else {
            panic!()
        };
        assert_eq!(
            main_class,
            "net.fabricmc.loader.impl.launch.knot.KnotServer"
        );
    }

    #[test]
    fn sha1_is_captured_as_the_upstream_verification_hash() {
        // Fabric publishes sha1 only, which is precisely why verify and content are separate.
        let s = parse_server_profile(
            profile_json().to_string().as_bytes(),
            "0.17.2",
            &server_jar(),
        )
        .unwrap();
        assert_eq!(
            s.libraries[0].verify.as_ref().unwrap().algo(),
            HashAlgo::Sha1
        );
    }

    #[test]
    fn a_library_without_a_hash_is_still_usable() {
        // Some entries omit sha1; we install them and record what we observed instead.
        let json = serde_json::json!({
            "id": "x", "mainClass": "M",
            "libraries": [{ "name": "a.b:c:1.0", "url": "https://repo1.maven.org/maven2/" }]
        });
        let s = parse_server_profile(json.to_string().as_bytes(), "1", &server_jar()).unwrap();
        assert!(s.libraries[0].verify.is_none());
        assert!(s.libraries[0].size.is_none());
    }

    #[test]
    fn a_library_with_no_repository_is_refused_rather_than_guessed() {
        let json = serde_json::json!({
            "id": "x", "mainClass": "M",
            "libraries": [{ "name": "a.b:c:1.0", "sha1": SHA1 }]
        });
        assert!(matches!(
            parse_server_profile(json.to_string().as_bytes(), "1", &server_jar()).unwrap_err(),
            FabricError::NoRepository(_)
        ));
    }

    #[test]
    fn a_hostile_library_coordinate_is_refused() {
        // Loader metadata arrives over the network and gets no more trust than a pack does.
        let json = serde_json::json!({
            "id": "x", "mainClass": "M",
            "libraries": [{ "name": "../../etc:passwd:1.0", "url": "https://x/" }]
        });
        assert!(matches!(
            parse_server_profile(json.to_string().as_bytes(), "1", &server_jar()).unwrap_err(),
            FabricError::BadLibrary { .. }
        ));
    }

    #[test]
    fn prefers_the_newest_stable_loader() {
        let json = serde_json::json!([
            { "loader": { "version": "0.18.0-beta.1", "stable": false } },
            { "loader": { "version": "0.17.2", "stable": true } },
            { "loader": { "version": "0.17.1", "stable": true } }
        ]);
        let v = choose_loader(json.to_string().as_bytes(), &MinecraftVersion::new("26.3")).unwrap();
        assert_eq!(v, "0.17.2");
    }

    #[test]
    fn falls_back_to_the_newest_build_when_none_is_stable() {
        let json = serde_json::json!([
            { "loader": { "version": "0.18.0-beta.2", "stable": false } },
            { "loader": { "version": "0.18.0-beta.1", "stable": false } }
        ]);
        let v = choose_loader(json.to_string().as_bytes(), &MinecraftVersion::new("26.3")).unwrap();
        assert_eq!(v, "0.18.0-beta.2", "metadata is newest-first");
    }

    #[test]
    fn an_empty_loader_list_is_an_error() {
        let err = choose_loader(b"[]", &MinecraftVersion::new("26.3")).unwrap_err();
        assert!(matches!(err, FabricError::NoLoader(_)));
    }

    #[test]
    fn urls_are_built_from_opaque_version_strings() {
        let mc = MinecraftVersion::new("26.3");
        assert_eq!(
            server_profile_url(&mc, "0.17.2"),
            "https://meta.fabricmc.net/v2/versions/loader/26.3/0.17.2/server/json"
        );
        assert_eq!(
            loaders_url(&mc),
            "https://meta.fabricmc.net/v2/versions/loader/26.3"
        );
    }
}
