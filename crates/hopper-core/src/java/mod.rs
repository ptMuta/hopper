//! Finding or fetching a JVM.
//!
//! The required major version is never guessed: it comes from `javaVersion.majorVersion` in
//! Mojang's per-version metadata (see [`crate::api::mojang`]). This module decides *which* JVM
//! satisfies that, preferring one already on the machine and only downloading when nothing
//! suitable exists.
//!
//! There is a bootstrap ordering constraint worth stating: Forge, NeoForge and Quilt are
//! installed by running a vendor jar, so a JVM has to exist before the loader does. Fabric is
//! pure metadata and needs no JVM until the server actually starts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod install;
pub mod probe;

pub use probe::{ProbeError, SystemJava, parse_probe_output};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// The machine we are running on, or `None` on a target we have no JVM story for.
    pub fn current() -> Option<Self> {
        let os = match std::env::consts::OS {
            "linux" => Os::Linux,
            "macos" => Os::MacOs,
            "windows" => Os::Windows,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X64,
            "aarch64" => Arch::Aarch64,
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// How Oracle and the GraalVM community builds spell this platform in a filename.
    fn graal_slug(self) -> &'static str {
        match (self.os, self.arch) {
            (Os::Linux, Arch::X64) => "linux-x64",
            (Os::Linux, Arch::Aarch64) => "linux-aarch64",
            (Os::MacOs, Arch::X64) => "macos-x64",
            (Os::MacOs, Arch::Aarch64) => "macos-aarch64",
            (Os::Windows, Arch::X64) => "windows-x64",
            (Os::Windows, Arch::Aarch64) => "windows-aarch64",
        }
    }

    fn adoptium_os(self) -> &'static str {
        match self.os {
            Os::Linux => "linux",
            Os::MacOs => "mac",
            Os::Windows => "windows",
        }
    }

    fn adoptium_arch(self) -> &'static str {
        match self.arch {
            Arch::X64 => "x64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// Archives are zip on Windows and tar.gz everywhere else.
    pub fn archive_ext(self) -> &'static str {
        match self.os {
            Os::Windows => "zip",
            _ => "tar.gz",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JavaVendor {
    /// Oracle GraalVM under the GraalVM Free Terms and Conditions.
    GraalVm,
    /// GraalVM Community Edition, GPLv2 with Classpath Exception.
    GraalVmCe,
    /// Eclipse Temurin.
    Adoptium,
}

impl JavaVendor {
    pub const fn name(self) -> &'static str {
        match self {
            Self::GraalVm => "GraalVM",
            Self::GraalVmCe => "GraalVM CE",
            Self::Adoptium => "Temurin",
        }
    }
}

impl std::fmt::Display for JavaVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// GraalVM lines with a clean free-redistribution story and long-term support.
///
/// Deliberately excludes the monthly innovation releases (25.1, 25.2, …): they supersede each
/// other with no backports, which is the wrong shape for something a server runs for months.
pub const GRAALVM_LTS: &[u32] = &[21, 25];

/// Which vendors to try, in order, for a given requirement.
///
/// The platform gaps here are real and current, and encoding them as code rather than as a
/// README caveat is the difference between a clear message and a confusing 404:
///
/// * GraalVM dropped macOS x64 after 25.0.1, so an Intel Mac needs Temurin for Java 25+.
/// * Neither vendor ships a Windows ARM build for the current feature versions.
/// * Below Java 21, Oracle's free-licence lines do not apply, so Temurin is the default.
pub fn vendor_chain(
    preferred: Option<JavaVendor>,
    major: u32,
    platform: Platform,
) -> Vec<JavaVendor> {
    if let Some(v) = preferred {
        return vec![v];
    }
    if !GRAALVM_LTS.contains(&major) {
        return vec![JavaVendor::Adoptium];
    }
    match (platform.os, platform.arch) {
        // GraalVM 25+ has no Intel Mac build at all.
        (Os::MacOs, Arch::X64) if major >= 25 => vec![JavaVendor::Adoptium],
        (Os::Windows, Arch::Aarch64) => vec![JavaVendor::Adoptium],
        _ => vec![
            JavaVendor::GraalVm,
            JavaVendor::GraalVmCe,
            JavaVendor::Adoptium,
        ],
    }
}

/// Whether a vendor publishes a build at all for this combination.
pub fn vendor_supports(vendor: JavaVendor, major: u32, platform: Platform) -> bool {
    match vendor {
        JavaVendor::GraalVm | JavaVendor::GraalVmCe => {
            if !GRAALVM_LTS.contains(&major) {
                return false;
            }
            !matches!((platform.os, platform.arch), (Os::Windows, Arch::Aarch64))
                && !(platform.os == Os::MacOs && platform.arch == Arch::X64 && major >= 25)
        }
        JavaVendor::Adoptium => !matches!(
            (platform.os, platform.arch, major),
            // Temurin ships no Windows ARM build for the newest feature versions.
            (Os::Windows, Arch::Aarch64, 25..)
        ),
    }
}

/// Where to fetch a JVM, and how to verify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCandidate {
    pub vendor: JavaVendor,
    pub major: u32,
    pub platform: Platform,
    pub url: String,
    /// Where the vendor publishes the checksum, when it is a separate file. Adoptium returns it
    /// inline in the same API response, so no second request is needed there.
    pub checksum_url: Option<String>,
}

/// Oracle's script-friendly "latest for this feature version" URL.
pub fn graalvm_url(major: u32, platform: Platform) -> String {
    format!(
        "https://download.oracle.com/graalvm/{major}/latest/graalvm-jdk-{major}_{}_bin.{}",
        platform.graal_slug(),
        platform.archive_ext()
    )
}

/// The Adoptium assets endpoint.
///
/// Preferred over `/v3/binary/latest/...` because the assets response carries the checksum
/// inline, so acquiring and verifying costs one round trip instead of two.
pub fn adoptium_assets_url(major: u32, platform: Platform, jre: bool) -> String {
    format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?os={}&architecture={}&image_type={}",
        platform.adoptium_os(),
        platform.adoptium_arch(),
        if jre { "jre" } else { "jdk" }
    )
}

pub fn candidate_for(vendor: JavaVendor, major: u32, platform: Platform) -> Option<JavaCandidate> {
    if !vendor_supports(vendor, major, platform) {
        return None;
    }
    let (url, checksum_url) = match vendor {
        JavaVendor::GraalVm => {
            let u = graalvm_url(major, platform);
            let c = format!("{u}.sha256");
            (u, Some(c))
        }
        // Resolved through the GitHub releases API at fetch time, since the asset filename
        // embeds a build number we cannot construct.
        JavaVendor::GraalVmCe => (
            "https://api.github.com/repos/graalvm/graalvm-ce-builds/releases".to_owned(),
            None,
        ),
        JavaVendor::Adoptium => (adoptium_assets_url(major, platform, true), None),
    };
    Some(JavaCandidate {
        vendor,
        major,
        platform,
        url,
        checksum_url,
    })
}

/// Why a particular Java version is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaReason {
    /// From Mojang's per-version metadata — the authoritative source.
    Minecraft { version: String },
    /// A loader needs something newer than Minecraft does.
    LoaderFloor { loader: String },
    /// The operator pinned it.
    Pinned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaRequirement {
    pub major: u32,
    pub reason: JavaReason,
}

impl JavaRequirement {
    /// Combine Minecraft's floor with a loader's, taking whichever is higher.
    pub fn combine(minecraft: JavaRequirement, loader_floor: Option<(u32, String)>) -> Self {
        match loader_floor {
            Some((floor, loader)) if floor > minecraft.major => Self {
                major: floor,
                reason: JavaReason::LoaderFloor { loader },
            },
            _ => minecraft,
        }
    }

    /// Whether an installed JVM satisfies this requirement.
    ///
    /// Exact match by default rather than "anything newer": Mojang test against the version
    /// they name, and older loaders genuinely break on newer JDKs.
    pub fn satisfied_by(&self, installed_major: u32) -> bool {
        installed_major == self.major
    }
}

/// What we decided to do about Java.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaPlan {
    /// A suitable JVM already exists — nothing is downloaded.
    UseExisting { java: SystemJava },
    /// Fetch one.
    Provision { candidate: JavaCandidate },
    /// Nothing available for this platform.
    Unavailable { major: u32, platform: Platform },
}

/// Choose a JVM, preferring what is already installed.
pub fn plan_java(
    req: &JavaRequirement,
    installed: &[SystemJava],
    preferred: Option<JavaVendor>,
    platform: Platform,
) -> JavaPlan {
    if let Some(java) = installed
        .iter()
        .find(|j| req.satisfied_by(j.major))
        .cloned()
    {
        return JavaPlan::UseExisting { java };
    }
    for vendor in vendor_chain(preferred, req.major, platform) {
        if let Some(candidate) = candidate_for(vendor, req.major, platform) {
            return JavaPlan::Provision { candidate };
        }
    }
    JavaPlan::Unavailable {
        major: req.major,
        platform,
    }
}

/// Where downloaded toolchains live.
///
/// The data directory, not the cache directory, and deliberately so: a cache cleaner running
/// over `~/.cache` must not brick every server on the machine. The *archives* stay in the
/// cache, where they genuinely are disposable.
pub fn toolchain_dir(
    data_root: &std::path::Path,
    vendor: JavaVendor,
    major: u32,
    platform: Platform,
) -> PathBuf {
    data_root.join("toolchains").join(format!(
        "{}-{}-{}",
        match vendor {
            JavaVendor::GraalVm => "graalvm",
            JavaVendor::GraalVmCe => "graalvm-ce",
            JavaVendor::Adoptium => "temurin",
        },
        major,
        platform.graal_slug()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: Platform = Platform {
        os: Os::Linux,
        arch: Arch::X64,
    };
    const LINUX_ARM: Platform = Platform {
        os: Os::Linux,
        arch: Arch::Aarch64,
    };
    const MAC_INTEL: Platform = Platform {
        os: Os::MacOs,
        arch: Arch::X64,
    };
    const MAC_ARM: Platform = Platform {
        os: Os::MacOs,
        arch: Arch::Aarch64,
    };
    const WIN_ARM: Platform = Platform {
        os: Os::Windows,
        arch: Arch::Aarch64,
    };

    fn installed(major: u32) -> SystemJava {
        SystemJava {
            path: PathBuf::from("/usr/bin/java"),
            major,
            version: format!("{major}.0.1"),
            vendor: "Eclipse Adoptium".into(),
            arch: "amd64".into(),
        }
    }

    #[test]
    fn graalvm_is_preferred_on_supported_lts_lines() {
        assert_eq!(vendor_chain(None, 21, LINUX)[0], JavaVendor::GraalVm);
        assert_eq!(vendor_chain(None, 25, LINUX)[0], JavaVendor::GraalVm);
    }

    #[test]
    fn non_lts_versions_fall_back_to_temurin() {
        // 25.1/25.2/25.3 are rolling innovation builds; 17 and 8 predate the free-licence lines.
        for major in [8, 11, 17, 22, 24] {
            assert_eq!(
                vendor_chain(None, major, LINUX),
                vec![JavaVendor::Adoptium],
                "Java {major}"
            );
        }
    }

    #[test]
    fn intel_macs_get_temurin_for_java_25_because_graalvm_dropped_them() {
        // A real 2026 platform cliff: GraalVM 25.0.1 was the last macOS x64 build.
        assert_eq!(
            vendor_chain(None, 25, MAC_INTEL),
            vec![JavaVendor::Adoptium]
        );
        // Java 21 still has one.
        assert_eq!(vendor_chain(None, 21, MAC_INTEL)[0], JavaVendor::GraalVm);
        // Apple Silicon is unaffected.
        assert_eq!(vendor_chain(None, 25, MAC_ARM)[0], JavaVendor::GraalVm);
    }

    #[test]
    fn arm_linux_is_fully_supported_since_that_is_the_common_arm_server() {
        assert_eq!(vendor_chain(None, 21, LINUX_ARM)[0], JavaVendor::GraalVm);
        assert!(vendor_supports(JavaVendor::GraalVm, 21, LINUX_ARM));
        assert!(vendor_supports(JavaVendor::Adoptium, 21, LINUX_ARM));
    }

    #[test]
    fn windows_on_arm_has_no_graalvm_build() {
        assert_eq!(vendor_chain(None, 21, WIN_ARM), vec![JavaVendor::Adoptium]);
        assert!(!vendor_supports(JavaVendor::GraalVm, 21, WIN_ARM));
    }

    #[test]
    fn an_explicit_vendor_choice_is_honoured_exactly() {
        assert_eq!(
            vendor_chain(Some(JavaVendor::Adoptium), 21, LINUX),
            vec![JavaVendor::Adoptium]
        );
    }

    #[test]
    fn oracle_url_matches_the_documented_pattern() {
        assert_eq!(
            graalvm_url(21, LINUX),
            "https://download.oracle.com/graalvm/21/latest/graalvm-jdk-21_linux-x64_bin.tar.gz"
        );
        assert_eq!(
            graalvm_url(25, MAC_ARM),
            "https://download.oracle.com/graalvm/25/latest/graalvm-jdk-25_macos-aarch64_bin.tar.gz"
        );
    }

    #[test]
    fn windows_downloads_are_zips() {
        assert!(
            graalvm_url(
                21,
                Platform {
                    os: Os::Windows,
                    arch: Arch::X64
                }
            )
            .ends_with("_bin.zip")
        );
    }

    #[test]
    fn graalvm_checksums_are_a_sibling_file() {
        let c = candidate_for(JavaVendor::GraalVm, 21, LINUX).unwrap();
        assert_eq!(c.checksum_url.unwrap(), format!("{}.sha256", c.url));
    }

    #[test]
    fn adoptium_uses_the_assets_endpoint_so_the_checksum_arrives_inline() {
        let url = adoptium_assets_url(21, LINUX, true);
        assert!(url.starts_with("https://api.adoptium.net/v3/assets/latest/21/hotspot"));
        assert!(url.contains("os=linux"));
        assert!(url.contains("architecture=x64"));
        assert!(url.contains("image_type=jre"));
        // Not the redirect endpoint, which would cost a second request for the checksum.
        assert!(!url.contains("/binary/"));
    }

    #[test]
    fn an_unsupported_combination_yields_no_candidate() {
        assert!(candidate_for(JavaVendor::GraalVm, 25, MAC_INTEL).is_none());
        assert!(candidate_for(JavaVendor::GraalVm, 17, LINUX).is_none());
    }

    #[test]
    fn an_existing_jvm_means_nothing_is_downloaded() {
        let req = JavaRequirement {
            major: 21,
            reason: JavaReason::Minecraft {
                version: "1.21.1".into(),
            },
        };
        let plan = plan_java(&req, &[installed(21)], None, LINUX);
        assert!(matches!(plan, JavaPlan::UseExisting { .. }), "{plan:?}");
    }

    #[test]
    fn a_wrong_version_on_the_machine_does_not_count() {
        let req = JavaRequirement {
            major: 25,
            reason: JavaReason::Minecraft {
                version: "26.3".into(),
            },
        };
        // Java 21 does not satisfy a Java 25 requirement, and neither would 26.
        let plan = plan_java(&req, &[installed(21), installed(26)], None, LINUX);
        assert!(matches!(plan, JavaPlan::Provision { .. }), "{plan:?}");
    }

    #[test]
    fn requirement_matching_is_exact_not_at_least() {
        // Mojang test against the version they name, and older loaders break on newer JDKs.
        let req = JavaRequirement {
            major: 21,
            reason: JavaReason::Pinned,
        };
        assert!(req.satisfied_by(21));
        assert!(!req.satisfied_by(25));
        assert!(!req.satisfied_by(17));
    }

    #[test]
    fn a_loader_floor_can_raise_but_never_lower_the_requirement() {
        let mc = JavaRequirement {
            major: 17,
            reason: JavaReason::Minecraft {
                version: "1.20.4".into(),
            },
        };
        let raised = JavaRequirement::combine(mc.clone(), Some((21, "neoforge".into())));
        assert_eq!(raised.major, 21);
        assert!(matches!(raised.reason, JavaReason::LoaderFloor { .. }));

        let unchanged = JavaRequirement::combine(mc.clone(), Some((11, "forge".into())));
        assert_eq!(unchanged.major, 17);
        assert!(matches!(unchanged.reason, JavaReason::Minecraft { .. }));

        assert_eq!(JavaRequirement::combine(mc, None).major, 17);
    }

    #[test]
    fn an_impossible_platform_is_reported_rather_than_attempted() {
        let req = JavaRequirement {
            major: 25,
            reason: JavaReason::Pinned,
        };
        // Force GraalVM where it does not exist.
        let plan = plan_java(&req, &[], Some(JavaVendor::GraalVm), MAC_INTEL);
        assert!(matches!(plan, JavaPlan::Unavailable { .. }), "{plan:?}");
    }

    #[test]
    fn toolchains_live_in_the_data_directory_not_the_cache() {
        // A cache cleaner must not be able to brick every server on the box.
        let dir = toolchain_dir(
            std::path::Path::new("/home/u/.local/share/hopper"),
            JavaVendor::GraalVm,
            21,
            LINUX,
        );
        let s = dir.to_string_lossy();
        assert!(s.contains("toolchains/graalvm-21-linux-x64"), "got {s}");
        assert!(!s.contains(".cache"));
    }
}

/// Resolving a vendor's download into a concrete URL and checksum.
pub mod fetch {
    use serde::Deserialize;

    use super::{JavaCandidate, JavaVendor};
    use crate::model::{Digest, HashAlgo};

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum ResolveError {
        #[error("{vendor} published no build for Java {major} on this platform")]
        NoBuild { vendor: JavaVendor, major: u32 },
        #[error("could not read {vendor}'s download metadata: {message}")]
        BadMetadata { vendor: JavaVendor, message: String },
        #[error("{vendor} published a malformed checksum")]
        BadChecksum { vendor: JavaVendor },
    }

    /// Where to download from, and what the bytes must hash to.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Download {
        pub url: String,
        pub checksum: Option<Digest>,
        pub size: Option<u64>,
    }

    /// Oracle publishes checksums as a sibling file containing `<hex>  <filename>`.
    pub fn parse_sha256_sidecar(body: &str, vendor: JavaVendor) -> Result<Digest, ResolveError> {
        let hex = body
            .split_whitespace()
            .next()
            .ok_or(ResolveError::BadChecksum { vendor })?;
        Digest::new(HashAlgo::Sha256, hex).map_err(|_| ResolveError::BadChecksum { vendor })
    }

    #[derive(Debug, Deserialize)]
    struct AdoptiumAsset {
        binary: AdoptiumBinary,
    }

    #[derive(Debug, Deserialize)]
    struct AdoptiumBinary {
        package: AdoptiumPackage,
    }

    #[derive(Debug, Deserialize)]
    struct AdoptiumPackage {
        link: String,
        #[serde(default)]
        checksum: Option<String>,
        #[serde(default)]
        size: Option<u64>,
    }

    /// Read Adoptium's assets response.
    ///
    /// The assets endpoint is used rather than the redirect one precisely because the checksum
    /// arrives inline here, so acquiring and verifying costs one round trip instead of two.
    pub fn parse_adoptium_assets(body: &[u8], major: u32) -> Result<Download, ResolveError> {
        let assets: Vec<AdoptiumAsset> =
            serde_json::from_slice(body).map_err(|e| ResolveError::BadMetadata {
                vendor: JavaVendor::Adoptium,
                message: e.to_string(),
            })?;
        let first = assets.into_iter().next().ok_or(ResolveError::NoBuild {
            vendor: JavaVendor::Adoptium,
            major,
        })?;
        let checksum = first
            .binary
            .package
            .checksum
            .as_deref()
            .and_then(|hex| Digest::new(HashAlgo::Sha256, hex).ok());
        Ok(Download {
            url: first.binary.package.link,
            checksum,
            size: first.binary.package.size,
        })
    }

    /// The Oracle GraalVM download for a candidate.
    pub fn graalvm_download(candidate: &JavaCandidate) -> Download {
        Download {
            url: candidate.url.clone(),
            // Fetched separately from the `.sha256` sidecar.
            checksum: None,
            size: None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

        #[test]
        fn reads_oracles_checksum_sidecar() {
            let body = format!("{SHA256}  graalvm-jdk-21_linux-x64_bin.tar.gz\n");
            let d = parse_sha256_sidecar(&body, JavaVendor::GraalVm).unwrap();
            assert_eq!(d.algo(), HashAlgo::Sha256);
            assert_eq!(d.hex(), SHA256);
        }

        #[test]
        fn a_bare_checksum_with_no_filename_still_parses() {
            assert!(parse_sha256_sidecar(SHA256, JavaVendor::GraalVm).is_ok());
        }

        #[test]
        fn a_malformed_checksum_is_refused() {
            for body in ["", "not-hex-at-all", "abc123"] {
                assert!(
                    parse_sha256_sidecar(body, JavaVendor::GraalVm).is_err(),
                    "{body:?} should be refused"
                );
            }
        }

        #[test]
        fn reads_adoptiums_inline_checksum() {
            let body = serde_json::json!([{
                "binary": {
                    "package": {
                        "name": "OpenJDK21U-jre_x64_linux_hotspot.tar.gz",
                        "link": "https://github.com/adoptium/temurin21-binaries/releases/download/x/y.tar.gz",
                        "checksum": SHA256,
                        "size": 45000000
                    }
                }
            }]);
            let d = parse_adoptium_assets(body.to_string().as_bytes(), 21).unwrap();
            assert!(d.url.contains("temurin21-binaries"));
            assert_eq!(d.checksum.unwrap().hex(), SHA256);
            assert_eq!(d.size, Some(45_000_000));
        }

        #[test]
        fn an_empty_adoptium_response_means_no_build_for_this_platform() {
            let err = parse_adoptium_assets(b"[]", 25).unwrap_err();
            assert!(matches!(err, ResolveError::NoBuild { major: 25, .. }));
        }

        #[test]
        fn a_missing_checksum_does_not_fail_the_parse() {
            // Verification is then by size alone, which the caller decides about.
            let body = serde_json::json!([{
                "binary": { "package": { "link": "https://example.test/x.tar.gz" } }
            }]);
            let d = parse_adoptium_assets(body.to_string().as_bytes(), 21).unwrap();
            assert!(d.checksum.is_none());
        }

        #[test]
        fn malformed_adoptium_json_is_reported() {
            assert!(matches!(
                parse_adoptium_assets(b"{not json", 21).unwrap_err(),
                ResolveError::BadMetadata { .. }
            ));
        }
    }
}
