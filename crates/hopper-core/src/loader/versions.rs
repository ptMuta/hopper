//! Discovering which loader builds exist.
//!
//! NeoForge publishes Maven metadata XML; Forge publishes a small JSON promotions file. Neither
//! is semver, so ordering comes from the upstream document rather than from parsing.

use serde::Deserialize;

use crate::model::MinecraftVersion;

pub const NEOFORGE_MAVEN_METADATA: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
/// NeoForge's 1.20.1 builds, published under Forge's artifact name before the rename.
pub const NEOFORGE_LEGACY_MAVEN_METADATA: &str =
    "https://maven.neoforged.net/releases/net/neoforged/forge/maven-metadata.xml";
pub const FORGE_PROMOTIONS: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("loader version metadata is malformed: {0}")]
    Malformed(String),
    #[error("no NeoForge build found for Minecraft {0}")]
    NoNeoForgeBuild(MinecraftVersion),
    #[error("no Forge build found for Minecraft {0}")]
    NoForgeBuild(MinecraftVersion),
}

/// Every version in a Maven metadata document, in document order.
pub fn parse_maven_metadata(xml: &[u8]) -> Result<Vec<String>, VersionError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut in_version = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"version" => in_version = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"version" => in_version = false,
            Ok(Event::Text(e)) if in_version => {
                let text = e
                    .unescape()
                    .map_err(|err| VersionError::Malformed(err.to_string()))?;
                out.push(text.into_owned());
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(VersionError::Malformed(e.to_string())),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// NeoForge builds for a Minecraft version, newest first.
///
/// NeoForge version numbers encode the Minecraft version they target: Minecraft `1.21.1`
/// becomes the `21.1.x` series — the leading `1.` is dropped and the next two components become
/// the major and minor. That is a documented NeoForge convention, not a guess, but it only
/// applies to the legacy scheme, so a caller must be prepared for an empty result on
/// year-based Minecraft versions until NeoForge publishes for them.
pub fn neoforge_versions_for(all: &[String], mc: &MinecraftVersion) -> Vec<String> {
    let Some(prefix) = neoforge_prefix(mc) else {
        return Vec::new();
    };
    let mut matching: Vec<String> = all
        .iter()
        .filter(|v| v.starts_with(&prefix))
        .cloned()
        .collect();
    // Maven metadata is oldest-first.
    matching.reverse();
    matching
}

/// NeoForge builds for 1.20.1, newest first, from [`NEOFORGE_LEGACY_MAVEN_METADATA`].
///
/// They are versioned `1.20.1-47.1.x`; the part after the Minecraft version is the build.
pub fn neoforge_legacy_versions(all: &[String]) -> Vec<String> {
    let mut out: Vec<String> = all
        .iter()
        .filter_map(|v| v.strip_prefix("1.20.1-"))
        .map(str::to_owned)
        .collect();
    out.reverse();
    out
}

/// `1.21.1` -> `21.1.`, `1.21` -> `21.0.`
fn neoforge_prefix(mc: &MinecraftVersion) -> Option<String> {
    let rest = mc.as_str().strip_prefix("1.")?;
    let mut parts = rest.split('.');
    let major = parts.next()?;
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minor = parts.next().unwrap_or("0");
    Some(format!("{major}.{minor}."))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForgePromotions {
    #[serde(default)]
    pub promos: std::collections::BTreeMap<String, String>,
}

impl ForgePromotions {
    pub fn parse(bytes: &[u8]) -> Result<Self, VersionError> {
        serde_json::from_slice(bytes).map_err(|e| VersionError::Malformed(e.to_string()))
    }

    /// The recommended build, falling back to the latest.
    ///
    /// Forge publishes both; recommended is the one to install by default, but plenty of
    /// Minecraft versions only ever get a latest.
    pub fn build_for(&self, mc: &MinecraftVersion) -> Option<&str> {
        self.promos
            .get(&format!("{}-recommended", mc.as_str()))
            .or_else(|| self.promos.get(&format!("{}-latest", mc.as_str())))
            .map(String::as_str)
    }

    /// The installer URL for a resolved build.
    pub fn installer_url(mc: &MinecraftVersion, build: &str) -> String {
        let v = format!("{}-{build}", mc.as_str());
        format!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/{v}/forge-{v}-installer.jar"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAVEN_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>net.neoforged</groupId>
  <artifactId>neoforge</artifactId>
  <versioning>
    <latest>21.1.95</latest>
    <release>21.1.95</release>
    <versions>
      <version>20.4.100</version>
      <version>21.1.80</version>
      <version>21.1.95</version>
      <version>21.4.10</version>
    </versions>
  </versioning>
</metadata>"#;

    #[test]
    fn reads_every_version_from_maven_metadata() {
        let v = parse_maven_metadata(MAVEN_XML).unwrap();
        assert_eq!(v, ["20.4.100", "21.1.80", "21.1.95", "21.4.10"]);
    }

    #[test]
    fn malformed_xml_is_an_error() {
        assert!(parse_maven_metadata(b"<metadata><versions>").is_ok());
        assert!(parse_maven_metadata(b"<<<not xml").is_err());
    }

    #[test]
    fn neoforge_versions_are_filtered_by_minecraft_series_newest_first() {
        let all = parse_maven_metadata(MAVEN_XML).unwrap();
        let v = neoforge_versions_for(&all, &MinecraftVersion::new("1.21.1"));
        assert_eq!(v, ["21.1.95", "21.1.80"], "newest first");
    }

    #[test]
    fn a_minecraft_version_with_no_neoforge_builds_yields_nothing() {
        let all = parse_maven_metadata(MAVEN_XML).unwrap();
        assert!(neoforge_versions_for(&all, &MinecraftVersion::new("1.19.2")).is_empty());
    }

    #[test]
    fn the_prefix_convention_drops_the_leading_one() {
        assert_eq!(
            neoforge_prefix(&MinecraftVersion::new("1.21.1")).as_deref(),
            Some("21.1.")
        );
        // A two-part version means patch zero.
        assert_eq!(
            neoforge_prefix(&MinecraftVersion::new("1.21")).as_deref(),
            Some("21.0.")
        );
    }

    #[test]
    fn year_based_versions_yield_no_prefix_rather_than_a_wrong_one() {
        // NeoForge's convention is defined against the legacy scheme only. Guessing here would
        // silently install a build for the wrong Minecraft version.
        assert_eq!(neoforge_prefix(&MinecraftVersion::new("26.3")), None);
        let all = parse_maven_metadata(MAVEN_XML).unwrap();
        assert!(neoforge_versions_for(&all, &MinecraftVersion::new("26.3")).is_empty());
    }

    #[test]
    fn neoforge_1_20_1_builds_come_from_the_legacy_artifact() {
        let all = vec!["1.20.1-47.1.3".to_owned(), "1.20.1-47.1.106".to_owned()];
        assert_eq!(neoforge_legacy_versions(&all), ["47.1.106", "47.1.3"]);
    }

    #[test]
    fn forge_prefers_recommended_over_latest() {
        let p = ForgePromotions::parse(
            serde_json::json!({
                "promos": {
                    "1.21.1-recommended": "52.0.40",
                    "1.21.1-latest": "52.1.0",
                    "1.20.4-latest": "49.1.0"
                }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();

        assert_eq!(
            p.build_for(&MinecraftVersion::new("1.21.1")),
            Some("52.0.40")
        );
        // Falls back when there is no recommended build.
        assert_eq!(
            p.build_for(&MinecraftVersion::new("1.20.4")),
            Some("49.1.0")
        );
        assert_eq!(p.build_for(&MinecraftVersion::new("1.7.10")), None);
    }

    #[test]
    fn forge_installer_url_matches_the_maven_layout() {
        assert_eq!(
            ForgePromotions::installer_url(&MinecraftVersion::new("1.21.1"), "52.0.40"),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.40/forge-1.21.1-52.0.40-installer.jar"
        );
    }
}
