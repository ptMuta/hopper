//! Opaque identifiers.
//!
//! The recurring bug these types exist to prevent: treating a version string as something you
//! can parse. Minecraft abandoned `1.x` numbering after 1.21.11 and now ships year-based
//! versions (`26.1`, `26.2`, `26.3`), so any code that regexes for a leading `1.` or compares
//! versions numerically is already wrong. Loader versions are not semver either. We therefore
//! treat all of them as opaque strings and resolve meaning through upstream metadata.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Defines a newtype over `String` that is comparable and printable but never parsed.
macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

opaque_id! {
    /// A registry project id (Modrinth base62, e.g. `AANobbMI`) or, where the API accepts it
    /// interchangeably, a slug.
    ProjectId
}

opaque_id! {
    /// A registry version id (e.g. `SMxNOGZ6`). Distinct from the human version *number*.
    VersionId
}

opaque_id! {
    /// A Minecraft version id, exactly as Mojang's manifest spells it.
    ///
    /// Opaque on purpose: `26.3` and `1.21.1` are both just strings, and ordering between them
    /// is manifest order, never string or numeric comparison.
    MinecraftVersion
}

/// Which registry a file came from. Present from the first lockfile so that adding CurseForge
/// later does not invalidate lockfiles written today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryId {
    Modrinth,
    CurseForge,
}

impl RegistryId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Modrinth => "Modrinth",
            Self::CurseForge => "CurseForge",
        }
    }
}

impl fmt::Display for RegistryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Quilt,
    NeoForge,
    Forge,
}

impl LoaderKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::NeoForge => "neoforge",
            Self::Forge => "forge",
        }
    }

    /// The key this loader uses in a `.mrpack`'s `dependencies` object.
    ///
    /// Note the asymmetry — Fabric and Quilt carry a `-loader` suffix and Forge and NeoForge do
    /// not. Matching is explicit rather than derived so an unrecognized key is an error instead
    /// of a guess.
    pub const fn mrpack_key(self) -> Option<&'static str> {
        match self {
            Self::Vanilla => None,
            Self::Fabric => Some("fabric-loader"),
            Self::Quilt => Some("quilt-loader"),
            Self::NeoForge => Some("neoforge"),
            Self::Forge => Some("forge"),
        }
    }

    pub fn from_mrpack_key(key: &str) -> Option<Self> {
        match key {
            "fabric-loader" => Some(Self::Fabric),
            "quilt-loader" => Some(Self::Quilt),
            "neoforge" => Some(Self::NeoForge),
            "forge" => Some(Self::Forge),
            _ => None,
        }
    }

    /// How Modrinth spells this loader in `loaders` query facets.
    pub const fn api_name(self) -> &'static str {
        self.name()
    }

    /// Whether installing this loader requires executing a vendor installer jar.
    ///
    /// Fabric is pure metadata (its meta API hands us a launch descriptor), while Forge and
    /// NeoForge patch the vanilla jar and must be run. This drives the JVM bootstrap ordering:
    /// a loader that needs a JVM to install forces Java provisioning to happen first.
    pub const fn needs_installer_jar(self) -> bool {
        match self {
            Self::Vanilla | Self::Fabric => false,
            Self::Quilt | Self::NeoForge | Self::Forge => true,
        }
    }
}

impl fmt::Display for LoaderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for LoaderKind {
    type Err = UnknownLoader;
    fn from_str(s: &str) -> Result<Self, UnknownLoader> {
        match s.to_ascii_lowercase().as_str() {
            "vanilla" | "none" => Ok(Self::Vanilla),
            "fabric" => Ok(Self::Fabric),
            "quilt" => Ok(Self::Quilt),
            "neoforge" => Ok(Self::NeoForge),
            "forge" => Ok(Self::Forge),
            _ => Err(UnknownLoader(s.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown mod loader {0:?} (expected fabric, neoforge, forge, quilt or vanilla)")]
pub struct UnknownLoader(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minecraft_versions_are_opaque_strings() {
        // The whole point: the 2026 year-based scheme and the legacy scheme coexist and neither
        // is parsed. If anyone reintroduces a `1.` assumption, it will not be here.
        for v in ["26.3", "1.21.1", "26.1", "1.7.10", "25w14craftmine"] {
            assert_eq!(MinecraftVersion::new(v).as_str(), v);
        }
    }

    #[test]
    fn minecraft_version_serializes_as_a_bare_string() {
        let v = MinecraftVersion::new("26.3");
        assert_eq!(serde_json::to_string(&v).unwrap(), r#""26.3""#);
        assert_eq!(
            serde_json::from_str::<MinecraftVersion>(r#""26.3""#).unwrap(),
            v
        );
    }

    #[test]
    fn mrpack_keys_round_trip() {
        for kind in [
            LoaderKind::Fabric,
            LoaderKind::Quilt,
            LoaderKind::NeoForge,
            LoaderKind::Forge,
        ] {
            let key = kind.mrpack_key().expect("modded loaders have a key");
            assert_eq!(LoaderKind::from_mrpack_key(key), Some(kind));
        }
        assert_eq!(LoaderKind::Vanilla.mrpack_key(), None);
    }

    #[test]
    fn mrpack_key_suffixes_are_not_uniform() {
        // Guards the exact spelling: guessing "neoforge-loader" here would silently fail to
        // detect the loader and install a vanilla server.
        assert_eq!(LoaderKind::Fabric.mrpack_key(), Some("fabric-loader"));
        assert_eq!(LoaderKind::NeoForge.mrpack_key(), Some("neoforge"));
        assert_eq!(LoaderKind::from_mrpack_key("minecraft"), None);
        assert_eq!(LoaderKind::from_mrpack_key("neoforge-loader"), None);
    }

    #[test]
    fn only_jar_patching_loaders_need_a_jvm_to_install() {
        assert!(!LoaderKind::Fabric.needs_installer_jar());
        assert!(!LoaderKind::Vanilla.needs_installer_jar());
        assert!(LoaderKind::NeoForge.needs_installer_jar());
        assert!(LoaderKind::Forge.needs_installer_jar());
        assert!(LoaderKind::Quilt.needs_installer_jar());
    }

    #[test]
    fn loader_parsing_is_case_insensitive() {
        assert_eq!(
            "NeoForge".parse::<LoaderKind>().unwrap(),
            LoaderKind::NeoForge
        );
        assert_eq!("FABRIC".parse::<LoaderKind>().unwrap(), LoaderKind::Fabric);
        assert!("babric".parse::<LoaderKind>().is_err());
    }
}
