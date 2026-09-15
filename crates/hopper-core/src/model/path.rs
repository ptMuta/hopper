//! `RelPath` — the single validation boundary for untrusted paths.
//!
//! Every path that reaches hopper from outside (a `.mrpack` index entry, a ZIP or tar entry
//! name, an API-supplied filename, a lockfile we did not write this run) is parsed here or it
//! does not become a path at all. There is deliberately no other constructor: making an
//! escaping path *unrepresentable* is cheaper to audit than remembering to check at each use.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a candidate path was refused.
///
/// These are security refusals, never warnings: callers must not downgrade them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path {0:?} is absolute")]
    Absolute(String),
    #[error("path {0:?} uses a backslash; pack paths must use '/'")]
    Backslash(String),
    #[error("path {0:?} has a drive letter")]
    DriveLetter(String),
    #[error("path {0:?} escapes the install directory")]
    Traversal(String),
    #[error("path {0:?} has an empty segment")]
    EmptySegment(String),
    #[error("path {0:?} ends with a separator")]
    TrailingSlash(String),
    #[error("path {0:?} contains a NUL byte")]
    Nul(String),
    #[error("path {0:?} contains a colon (alternate data stream)")]
    Colon(String),
    #[error("path {0:?} contains the reserved device name {1:?}")]
    ReservedName(String, String),
    #[error("path {0:?} has a segment ending in a dot or space")]
    TrailingDotOrSpace(String),
    #[error("path {0:?} is longer than {1} bytes")]
    TooLong(String, usize),
}

/// Paths longer than this are refused outright. Real pack entries are far shorter; anything
/// near the limit is a probe rather than a mod.
const MAX_LEN: usize = 1024;

/// Windows device names. These are reserved at *every* directory level and with any extension,
/// so `config/CON.toml` is still a trap. Refused on all platforms so a pack cannot be crafted
/// to behave differently on Windows than it did in review on Linux.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A validated pack-relative path: UTF-8, `/`-separated, strictly below the install root.
///
/// Ordering is byte order on the whole string, which gives stable, reviewable lockfile diffs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(Box<str>);

impl RelPath {
    /// The only way to build a `RelPath`.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let owned = || s.to_owned();

        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.len() > MAX_LEN {
            return Err(PathError::TooLong(owned(), MAX_LEN));
        }
        if s.contains('\0') {
            return Err(PathError::Nul(owned()));
        }
        // Checked before the separator scan: on Windows `..\..\x` traverses, and we must not
        // let a path that is inert on Linux become an escape on another platform.
        if s.contains('\\') {
            return Err(PathError::Backslash(owned()));
        }
        // Rejects both `C:/x` drive-relative paths and NTFS alternate data streams (`a.jar:evil`).
        if let Some(i) = s.find(':') {
            return Err(if i == 1 {
                PathError::DriveLetter(owned())
            } else {
                PathError::Colon(owned())
            });
        }
        if s.starts_with('/') {
            return Err(PathError::Absolute(owned()));
        }
        if s.ends_with('/') {
            return Err(PathError::TrailingSlash(owned()));
        }

        for seg in s.split('/') {
            if seg.is_empty() {
                return Err(PathError::EmptySegment(owned()));
            }
            if seg == "." || seg == ".." {
                return Err(PathError::Traversal(owned()));
            }
            // Windows silently strips these, so `evil. ` and `evil` would collide after a
            // round-trip through the filesystem.
            if seg.ends_with('.') || seg.ends_with(' ') {
                return Err(PathError::TrailingDotOrSpace(owned()));
            }
            let stem = seg.split('.').next().unwrap_or(seg);
            if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
                return Err(PathError::ReservedName(owned(), stem.to_owned()));
            }
        }

        Ok(Self(s.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The final segment. Never empty — a trailing slash is refused at parse time.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn parent(&self) -> Option<RelPath> {
        let (head, _) = self.0.rsplit_once('/')?;
        Some(Self(head.into()))
    }

    /// Every ancestor directory, deepest first. Used to prune directories emptied by a removal.
    pub fn ancestors(&self) -> impl Iterator<Item = RelPath> {
        let mut cur = self.parent();
        std::iter::from_fn(move || {
            let out = cur.clone();
            cur = cur.as_ref().and_then(RelPath::parent);
            out
        })
    }

    /// True when this path lies inside directory `dir` (segment-wise, so `mod` does not match
    /// `mods/`).
    pub fn starts_with_dir(&self, dir: &str) -> bool {
        let dir = dir.trim_end_matches('/');
        self.0.len() > dir.len() && self.0.as_bytes()[dir.len()] == b'/' && self.0.starts_with(dir)
    }

    /// Case-folded key for spotting two pack entries that would collide on a
    /// case-insensitive filesystem (APFS, NTFS) while looking distinct on ext4.
    pub fn fold_key(&self) -> String {
        self.0.to_lowercase()
    }

    /// Append a suffix to the whole path, e.g. `.disabled` or `.new`.
    ///
    /// Returns an error rather than panicking because the suffix can push the result past the
    /// length limit or reintroduce a reserved name.
    pub fn with_suffix(&self, suffix: &str) -> Result<RelPath, PathError> {
        RelPath::parse(&format!("{}{}", self.0, suffix))
    }

    /// Join under `root`. Infallible by construction: validation already happened, so this
    /// cannot escape. Kept as a method so no caller hand-rolls the join.
    pub fn resolve_under(&self, root: &std::path::Path) -> std::path::PathBuf {
        let mut p = root.to_path_buf();
        for seg in self.segments() {
            p.push(seg);
        }
        p
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelPath({:?})", &*self.0)
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

/// Deserialization routes through `parse`, so a hand-edited or hostile lockfile is held to
/// exactly the same rules as a downloaded pack.
impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        RelPath::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for RelPath {
    type Err = PathError;
    fn from_str(s: &str) -> Result<Self, PathError> {
        RelPath::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("mods/sodium.jar")]
    #[case("a")]
    #[case("config/some-mod/settings.toml")]
    #[case("libraries/net/fabricmc/fabric-loader/0.17.2/fabric-loader-0.17.2.jar")]
    #[case("mods/Not.A.Reserved.Name.jar")]
    #[case("consumer/data.txt")] // starts with "con" but is not the reserved name
    #[case("mods/sodium-fabric-0.8.13+mc1.21.1.jar")]
    #[case("config/résumé.toml")]
    fn accepts_ordinary_paths(#[case] s: &str) {
        assert!(RelPath::parse(s).is_ok(), "should accept {s:?}");
    }

    #[rstest]
    #[case("", PathError::Empty)]
    #[case("/etc/passwd", PathError::Absolute("/etc/passwd".into()))]
    #[case("mods/", PathError::TrailingSlash("mods/".into()))]
    #[case("mods//a.jar", PathError::EmptySegment("mods//a.jar".into()))]
    fn rejects_malformed(#[case] s: &str, #[case] want: PathError) {
        assert_eq!(RelPath::parse(s).unwrap_err(), want);
    }

    /// The traversal corpus. Each of these has been seen in the wild against archive
    /// extractors; none may ever parse.
    #[rstest]
    #[case("../etc/passwd")]
    #[case("..")]
    #[case(".")]
    #[case("mods/../../etc/passwd")]
    #[case("mods/./a.jar")]
    #[case("a/b/../../../c")]
    #[case("../../../../../../../../etc/shadow")]
    #[case("mods/..")]
    fn rejects_traversal(#[case] s: &str) {
        let err = RelPath::parse(s).unwrap_err();
        assert!(
            matches!(err, PathError::Traversal(_)),
            "{s:?} should be Traversal, got {err:?}"
        );
    }

    /// Windows-flavoured escapes. Refused on every platform: a pack must not be inert in
    /// review on Linux and hostile on a Windows host.
    #[rstest]
    #[case("..\\..\\windows\\system32\\cmd.exe")]
    #[case("mods\\a.jar")]
    fn rejects_backslash(#[case] s: &str) {
        assert!(matches!(
            RelPath::parse(s).unwrap_err(),
            PathError::Backslash(_)
        ));
    }

    #[test]
    fn rejects_drive_letter() {
        assert!(matches!(
            RelPath::parse("C:/windows/system32").unwrap_err(),
            PathError::DriveLetter(_)
        ));
    }

    #[test]
    fn rejects_alternate_data_stream() {
        assert!(matches!(
            RelPath::parse("mods/a.jar:payload").unwrap_err(),
            PathError::Colon(_)
        ));
    }

    #[test]
    fn rejects_nul() {
        assert!(matches!(
            RelPath::parse("mods/a\0.jar").unwrap_err(),
            PathError::Nul(_)
        ));
    }

    /// Reserved at any depth and with any extension — `config/CON.toml` is still a trap.
    #[rstest]
    #[case("CON")]
    #[case("config/CON.toml")]
    #[case("mods/nul.jar")]
    #[case("a/b/LPT9.txt")]
    #[case("com1")]
    fn rejects_reserved_device_names(#[case] s: &str) {
        assert!(
            matches!(RelPath::parse(s).unwrap_err(), PathError::ReservedName(..)),
            "{s:?} should be refused"
        );
    }

    /// Windows strips trailing dots and spaces, so `evil. ` and `evil` would alias.
    #[rstest]
    #[case("mods/evil.")]
    #[case("mods/evil ")]
    #[case("dir./a.jar")]
    fn rejects_trailing_dot_or_space(#[case] s: &str) {
        assert!(matches!(
            RelPath::parse(s).unwrap_err(),
            PathError::TrailingDotOrSpace(_)
        ));
    }

    #[test]
    fn rejects_overlong() {
        let s = format!("mods/{}.jar", "a".repeat(MAX_LEN));
        assert!(matches!(
            RelPath::parse(&s).unwrap_err(),
            PathError::TooLong(..)
        ));
    }

    #[test]
    fn resolve_under_stays_inside_root() {
        let root = std::path::Path::new("/srv/mc");
        let p = RelPath::parse("mods/sodium.jar").unwrap();
        assert_eq!(
            p.resolve_under(root),
            std::path::Path::new("/srv/mc/mods/sodium.jar")
        );
        // Whatever a parsed path contains, the join cannot leave the root.
        assert!(p.resolve_under(root).starts_with(root));
    }

    #[test]
    fn parent_and_ancestors() {
        let p = RelPath::parse("a/b/c.txt").unwrap();
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        let anc: Vec<String> = p.ancestors().map(|a| a.as_str().to_owned()).collect();
        assert_eq!(anc, vec!["a/b", "a"]);
        assert_eq!(RelPath::parse("top.txt").unwrap().parent(), None);
    }

    #[test]
    fn file_name_is_last_segment() {
        assert_eq!(RelPath::parse("a/b/c.txt").unwrap().file_name(), "c.txt");
        assert_eq!(RelPath::parse("solo.txt").unwrap().file_name(), "solo.txt");
    }

    #[test]
    fn starts_with_dir_is_segment_wise() {
        let p = RelPath::parse("mods/a.jar").unwrap();
        assert!(p.starts_with_dir("mods"));
        assert!(p.starts_with_dir("mods/"));
        // Must not match on a shared prefix that is not a directory boundary.
        assert!(!p.starts_with_dir("mod"));
        // A path is not inside itself.
        assert!(!RelPath::parse("mods").unwrap().starts_with_dir("mods"));
    }

    #[test]
    fn fold_key_spots_case_collisions() {
        let a = RelPath::parse("mods/Sodium.jar").unwrap();
        let b = RelPath::parse("mods/sodium.jar").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.fold_key(), b.fold_key());
    }

    #[test]
    fn with_suffix_revalidates() {
        let p = RelPath::parse("mods/a.jar").unwrap();
        assert_eq!(
            p.with_suffix(".disabled").unwrap().as_str(),
            "mods/a.jar.disabled"
        );
        // A suffix may not smuggle in an escape.
        assert!(p.with_suffix("/../../etc").is_err());
    }

    #[test]
    fn deserialize_enforces_the_same_rules() {
        // A hostile or hand-edited lockfile gets no special trust.
        let err = serde_json::from_str::<RelPath>(r#""../../etc/passwd""#).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");

        let ok: RelPath = serde_json::from_str(r#""mods/a.jar""#).unwrap();
        assert_eq!(ok.as_str(), "mods/a.jar");
    }

    #[test]
    fn serde_round_trips() {
        let p = RelPath::parse("config/a/b.toml").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#""config/a/b.toml""#);
        assert_eq!(serde_json::from_str::<RelPath>(&json).unwrap(), p);
    }

    #[test]
    fn ordering_is_stable_for_lockfile_diffs() {
        let mut v = ["mods/b.jar", "config/a.toml", "mods/a.jar"]
            .map(|s| RelPath::parse(s).unwrap())
            .to_vec();
        v.sort();
        let got: Vec<&str> = v.iter().map(RelPath::as_str).collect();
        assert_eq!(got, ["config/a.toml", "mods/a.jar", "mods/b.jar"]);
    }
}
