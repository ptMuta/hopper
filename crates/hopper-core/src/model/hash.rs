//! Content hashing.
//!
//! Every file hopper handles carries **two** digests with different jobs:
//!
//! * `verify` — whatever upstream vouched for. Modrinth publishes sha1 and sha512, but Fabric's
//!   meta API and Mojang's manifests publish sha1 only, so this is not always strong. It is used
//!   exactly once, at download time, to decide whether the bytes are the bytes we asked for.
//! * `content` — always sha512, computed by us over the final bytes. This is the lockfile
//!   identity and the content-addressed-store key.
//!
//! Keeping them separate is what lets us accept a sha1-only upstream without ever making sha1
//! our notion of identity. [`MultiHasher`] computes both in a single pass, so the split is free.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha1::Digest as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgo {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Length of the hex encoding, used to reject truncated or padded digests at parse time.
    const fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashError {
    #[error("hash {0:?} is missing an algorithm prefix (expected e.g. \"sha512:...\")")]
    NoPrefix(String),
    #[error("unknown hash algorithm {0:?}")]
    UnknownAlgo(String),
    #[error("{algo} digest must be {want} hex characters, got {got}")]
    BadLength {
        algo: HashAlgo,
        want: usize,
        got: usize,
    },
    #[error("digest {0:?} is not valid hexadecimal")]
    NotHex(String),
}

/// A content hash, serialized as `"sha512:9f86d0…"` — self-describing, and one JSON string
/// rather than a nested object, which keeps the lockfile readable.
///
/// The hex is stored lowercased so equality is a plain string compare.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algo: HashAlgo,
    hex: Box<str>,
}

impl Digest {
    /// Build from an algorithm and raw hex. Case-insensitive on input, normalized to lowercase.
    pub fn new(algo: HashAlgo, hex: &str) -> Result<Self, HashError> {
        if hex.len() != algo.hex_len() {
            return Err(HashError::BadLength {
                algo,
                want: algo.hex_len(),
                got: hex.len(),
            });
        }
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(HashError::NotHex(hex.to_owned()));
        }
        Ok(Self {
            algo,
            hex: hex.to_ascii_lowercase().into(),
        })
    }

    /// Parse the prefixed `"algo:hex"` form used in the lockfile and on the CLI.
    pub fn parse(s: &str) -> Result<Self, HashError> {
        let (algo, hex) = s
            .split_once(':')
            .ok_or_else(|| HashError::NoPrefix(s.to_owned()))?;
        let algo = match algo.to_ascii_lowercase().as_str() {
            "sha1" => HashAlgo::Sha1,
            "sha256" => HashAlgo::Sha256,
            "sha512" => HashAlgo::Sha512,
            other => return Err(HashError::UnknownAlgo(other.to_owned())),
        };
        Self::new(algo, hex)
    }

    pub fn algo(&self) -> HashAlgo {
        self.algo
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Two-level fanout for the content store, so no single directory holds every blob.
    pub fn shard(&self) -> (&str, &str) {
        (&self.hex[0..2], &self.hex[2..4])
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

/// Abbreviated so log lines and test failures stay readable; the full value is in `Display`.
impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}…", self.algo, &self.hex[..12.min(self.hex.len())])
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Digest {
    type Err = HashError;
    fn from_str(s: &str) -> Result<Self, HashError> {
        Digest::parse(s)
    }
}

/// The set of digests a source published for one file. All fields optional because upstreams
/// differ: Modrinth gives sha1 and sha512, Fabric and Mojang give sha1 alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha512: Option<Digest>,
}

impl Hashes {
    pub fn get(&self, algo: HashAlgo) -> Option<&Digest> {
        match algo {
            HashAlgo::Sha1 => self.sha1.as_ref(),
            HashAlgo::Sha256 => self.sha256.as_ref(),
            HashAlgo::Sha512 => self.sha512.as_ref(),
        }
    }

    pub fn set(&mut self, d: Digest) {
        match d.algo() {
            HashAlgo::Sha1 => self.sha1 = Some(d),
            HashAlgo::Sha256 => self.sha256 = Some(d),
            HashAlgo::Sha512 => self.sha512 = Some(d),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sha1.is_none() && self.sha256.is_none() && self.sha512.is_none()
    }

    /// The strongest digest present — what we verify against when we have a choice.
    pub fn strongest(&self) -> Option<&Digest> {
        self.sha512
            .as_ref()
            .or(self.sha256.as_ref())
            .or(self.sha1.as_ref())
    }

    /// Compare against observed digests.
    ///
    /// `Some(true)` every shared algorithm agreed, `Some(false)` at least one disagreed,
    /// `None` no algorithm in common, so this proves nothing and the caller must not treat it
    /// as success.
    pub fn verify(&self, observed: &Hashes) -> Option<bool> {
        let mut compared = false;
        for algo in [HashAlgo::Sha512, HashAlgo::Sha256, HashAlgo::Sha1] {
            if let (Some(a), Some(b)) = (self.get(algo), observed.get(algo)) {
                compared = true;
                if a != b {
                    return Some(false);
                }
            }
        }
        compared.then_some(true)
    }
}

/// Computes several digests over one pass of a byte stream.
///
/// sha512 is always computed because it is our content identity; sha1 is computed alongside it
/// because Modrinth's lookup endpoints are keyed by it and recomputing later would mean a
/// second full read of the file.
pub struct MultiHasher {
    sha1: Option<sha1::Sha1>,
    sha256: Option<sha2::Sha256>,
    sha512: sha2::Sha512,
}

impl MultiHasher {
    /// sha512 plus sha1 — the default for pack files.
    pub fn new() -> Self {
        Self {
            sha1: Some(sha1::Sha1::new()),
            sha256: None,
            sha512: sha2::Sha512::new(),
        }
    }

    /// sha512 only, for content we will never look up by sha1.
    pub fn content_only() -> Self {
        Self {
            sha1: None,
            sha256: None,
            sha512: sha2::Sha512::new(),
        }
    }

    /// Adds sha256, which JDK vendors publish.
    pub fn with_sha256(mut self) -> Self {
        self.sha256 = Some(sha2::Sha256::new());
        self
    }

    pub fn update(&mut self, buf: &[u8]) {
        if let Some(h) = self.sha1.as_mut() {
            h.update(buf);
        }
        if let Some(h) = self.sha256.as_mut() {
            h.update(buf);
        }
        self.sha512.update(buf);
    }

    /// Finish, returning every digest computed plus the sha512 content key separately, since
    /// callers almost always want that one specifically.
    pub fn finish(self) -> (Hashes, Digest) {
        let mut out = Hashes::default();
        if let Some(h) = self.sha1 {
            out.sha1 = Some(digest_of(HashAlgo::Sha1, &h.finalize()));
        }
        if let Some(h) = self.sha256 {
            out.sha256 = Some(digest_of(HashAlgo::Sha256, &h.finalize()));
        }
        let content = digest_of(HashAlgo::Sha512, &self.sha512.finalize());
        out.sha512 = Some(content.clone());
        (out, content)
    }
}

impl Default for MultiHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Digests produced here are well-formed by construction, so the length and hex checks in
/// `Digest::new` cannot fail.
fn digest_of(algo: HashAlgo, bytes: &[u8]) -> Digest {
    Digest::new(algo, &hex::encode(bytes)).expect("hasher output is valid hex of fixed length")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    const EMPTY_SHA512: &str = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";

    #[test]
    fn hashes_the_empty_input_correctly() {
        // Known-answer test: catches a wired-up-wrong hasher, which a round-trip test would miss.
        let (all, content) = MultiHasher::new().finish();
        assert_eq!(all.sha1.unwrap().hex(), EMPTY_SHA1);
        assert_eq!(content.hex(), EMPTY_SHA512);
        assert_eq!(content.algo(), HashAlgo::Sha512);
    }

    #[test]
    fn streaming_matches_a_single_update() {
        let mut chunked = MultiHasher::new();
        chunked.update(b"hello, ");
        chunked.update(b"world");
        let mut whole = MultiHasher::new();
        whole.update(b"hello, world");
        assert_eq!(chunked.finish().1, whole.finish().1);
    }

    #[test]
    fn content_only_skips_sha1() {
        let (all, _) = MultiHasher::content_only().finish();
        assert!(all.sha1.is_none());
        assert!(all.sha512.is_some());
    }

    #[test]
    fn parses_and_displays_prefixed_form() {
        let d = Digest::parse(&format!("sha1:{EMPTY_SHA1}")).unwrap();
        assert_eq!(d.algo(), HashAlgo::Sha1);
        assert_eq!(d.to_string(), format!("sha1:{EMPTY_SHA1}"));
    }

    #[test]
    fn normalizes_case_so_equality_is_a_string_compare() {
        let upper = Digest::new(HashAlgo::Sha1, &EMPTY_SHA1.to_uppercase()).unwrap();
        let lower = Digest::new(HashAlgo::Sha1, EMPTY_SHA1).unwrap();
        assert_eq!(upper, lower);
        assert_eq!(upper.hex(), EMPTY_SHA1);
    }

    #[rstest]
    #[case("deadbeef")] // no prefix
    #[case("md5:d41d8cd98f00b204e9800998ecf8427e")] // unsupported algorithm
    #[case("sha1:abc")] // too short
    #[case("sha1:zzz9a3ee5e6b4b0d3255bfef95601890afd80709")] // not hex
    fn rejects_malformed_digests(#[case] s: &str) {
        assert!(Digest::parse(s).is_err(), "should reject {s:?}");
    }

    #[test]
    fn length_is_enforced_per_algorithm() {
        // A sha1-length digest must not be accepted as sha512.
        assert!(matches!(
            Digest::new(HashAlgo::Sha512, EMPTY_SHA1).unwrap_err(),
            HashError::BadLength { .. }
        ));
    }

    #[test]
    fn shard_splits_the_leading_bytes() {
        let d = Digest::new(HashAlgo::Sha1, EMPTY_SHA1).unwrap();
        assert_eq!(d.shard(), ("da", "39"));
    }

    #[test]
    fn strongest_prefers_sha512() {
        let mut h = Hashes::default();
        h.set(Digest::new(HashAlgo::Sha1, EMPTY_SHA1).unwrap());
        assert_eq!(h.strongest().unwrap().algo(), HashAlgo::Sha1);
        h.set(Digest::new(HashAlgo::Sha512, EMPTY_SHA512).unwrap());
        assert_eq!(h.strongest().unwrap().algo(), HashAlgo::Sha512);
    }

    #[test]
    fn verify_reports_agreement_disagreement_and_ignorance() {
        let sha1 = Digest::new(HashAlgo::Sha1, EMPTY_SHA1).unwrap();
        let sha512 = Digest::new(HashAlgo::Sha512, EMPTY_SHA512).unwrap();

        let mut want = Hashes::default();
        want.set(sha1.clone());
        let mut got = Hashes::default();
        got.set(sha1.clone());
        assert_eq!(want.verify(&got), Some(true));

        let mut wrong = Hashes::default();
        wrong.set(Digest::new(HashAlgo::Sha1, &"a".repeat(40)).unwrap());
        assert_eq!(want.verify(&wrong), Some(false));

        // No shared algorithm proves nothing — this must NOT read as success.
        let mut other = Hashes::default();
        other.set(sha512);
        assert_eq!(want.verify(&other), None);
    }

    #[test]
    fn disagreement_on_any_shared_algorithm_fails_the_whole_comparison() {
        let mut want = Hashes::default();
        want.set(Digest::new(HashAlgo::Sha1, EMPTY_SHA1).unwrap());
        want.set(Digest::new(HashAlgo::Sha512, EMPTY_SHA512).unwrap());

        // sha512 agrees but sha1 does not: a collision attempt, not a pass.
        let mut got = Hashes::default();
        got.set(Digest::new(HashAlgo::Sha1, &"b".repeat(40)).unwrap());
        got.set(Digest::new(HashAlgo::Sha512, EMPTY_SHA512).unwrap());
        assert_eq!(want.verify(&got), Some(false));
    }

    #[test]
    fn serde_round_trips_as_one_string() {
        let d = Digest::new(HashAlgo::Sha512, EMPTY_SHA512).unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"sha512:{EMPTY_SHA512}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);
    }

    #[test]
    fn deserialize_rejects_a_malformed_digest() {
        assert!(serde_json::from_str::<Digest>(r#""sha1:nope""#).is_err());
    }

    #[test]
    fn empty_hashes_skip_serialization() {
        assert_eq!(serde_json::to_string(&Hashes::default()).unwrap(), "{}");
    }
}
