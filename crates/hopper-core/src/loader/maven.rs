//! Maven coordinates.
//!
//! Loader metadata names libraries as `group:artifact:version[:classifier]` plus a repository
//! base URL, and expects the client to derive both the download URL and the on-disk path. The
//! layout is shared by Fabric, Quilt, Forge and NeoForge, so it lives here once.

use crate::model::{PathError, RelPath};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MavenError {
    #[error("malformed maven coordinate {0:?}")]
    Malformed(String),
    #[error("maven coordinate {coord:?} produces an unusable path")]
    BadPath {
        coord: String,
        #[source]
        source: PathError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub classifier: Option<String>,
    pub extension: String,
}

impl Coordinate {
    pub fn parse(s: &str) -> Result<Self, MavenError> {
        // The extension is an optional `@ext` suffix, e.g. `a:b:1.0@zip`.
        let (body, extension) = match s.split_once('@') {
            Some((b, e)) if !e.is_empty() => (b, e.to_owned()),
            _ => (s, "jar".to_owned()),
        };

        let parts: Vec<&str> = body.split(':').collect();
        let bad = || MavenError::Malformed(s.to_owned());
        let (group, artifact, version) = match parts.as_slice() {
            [g, a, v] | [g, a, v, _] if !g.is_empty() && !a.is_empty() && !v.is_empty() => {
                ((*g).to_owned(), (*a).to_owned(), (*v).to_owned())
            }
            _ => return Err(bad()),
        };
        let classifier = match parts.as_slice() {
            [_, _, _, c] if !c.is_empty() => Some((*c).to_owned()),
            [_, _, _, _] => return Err(bad()),
            _ => None,
        };

        Ok(Self {
            group,
            artifact,
            version,
            classifier,
            extension,
        })
    }

    /// The repository-relative path: group dots become slashes.
    pub fn relative_path(&self) -> String {
        let group = self.group.replace('.', "/");
        let classifier = self
            .classifier
            .as_ref()
            .map(|c| format!("-{c}"))
            .unwrap_or_default();
        format!(
            "{group}/{}/{}/{}-{}{classifier}.{}",
            self.artifact, self.version, self.artifact, self.version, self.extension
        )
    }

    /// Where this library goes inside the server directory.
    pub fn install_path(&self) -> Result<RelPath, MavenError> {
        let p = format!("libraries/{}", self.relative_path());
        RelPath::parse(&p).map_err(|source| MavenError::BadPath {
            coord: format!("{}:{}:{}", self.group, self.artifact, self.version),
            source,
        })
    }

    /// Download URL against a repository base.
    pub fn url(&self, repo_base: &str) -> String {
        format!(
            "{}/{}",
            repo_base.trim_end_matches('/'),
            self.relative_path()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_three_part_coordinate() {
        let c = Coordinate::parse("net.fabricmc:fabric-loader:0.17.2").unwrap();
        assert_eq!(c.group, "net.fabricmc");
        assert_eq!(c.artifact, "fabric-loader");
        assert_eq!(c.version, "0.17.2");
        assert_eq!(c.classifier, None);
        assert_eq!(c.extension, "jar");
    }

    #[test]
    fn group_dots_become_directories() {
        let c = Coordinate::parse("net.fabricmc:fabric-loader:0.17.2").unwrap();
        assert_eq!(
            c.relative_path(),
            "net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar"
        );
        assert_eq!(
            c.install_path().unwrap().as_str(),
            "libraries/net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar"
        );
    }

    #[test]
    fn handles_a_classifier() {
        let c = Coordinate::parse("net.neoforged:neoforge:21.1.95:universal").unwrap();
        assert_eq!(c.classifier.as_deref(), Some("universal"));
        assert!(
            c.relative_path()
                .ends_with("neoforge-21.1.95-universal.jar")
        );
    }

    #[test]
    fn handles_an_explicit_extension() {
        let c = Coordinate::parse("net.neoforged:neoform:1.21.1@zip").unwrap();
        assert_eq!(c.extension, "zip");
        assert!(c.relative_path().ends_with("neoform-1.21.1.zip"));
    }

    #[test]
    fn handles_a_classifier_and_an_extension_together() {
        let c = Coordinate::parse("a.b:c:1.0:sources@zip").unwrap();
        assert_eq!(c.classifier.as_deref(), Some("sources"));
        assert_eq!(c.extension, "zip");
        assert!(c.relative_path().ends_with("c-1.0-sources.zip"));
    }

    #[test]
    fn builds_urls_against_a_repository_base() {
        let c = Coordinate::parse("net.fabricmc:fabric-loader:0.17.2").unwrap();
        // Trailing slash on the base must not double up.
        for base in ["https://maven.fabricmc.net/", "https://maven.fabricmc.net"] {
            assert_eq!(
                c.url(base),
                "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar"
            );
        }
    }

    #[test]
    fn rejects_malformed_coordinates() {
        for s in ["", "a", "a:b", "a:b:c:", ":b:c", "a::c", "a:b:"] {
            assert!(Coordinate::parse(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn a_hostile_coordinate_cannot_escape_the_libraries_directory() {
        // Loader metadata is fetched over the network, so it is untrusted input like any other.
        let c = Coordinate::parse("../../etc:passwd:1.0").unwrap();
        assert!(
            c.install_path().is_err(),
            "traversal in a group id must be refused"
        );
    }

    #[test]
    fn install_paths_land_under_libraries() {
        let c = Coordinate::parse("org.ow2.asm:asm:9.7").unwrap();
        assert!(c.install_path().unwrap().starts_with_dir("libraries"));
    }
}
