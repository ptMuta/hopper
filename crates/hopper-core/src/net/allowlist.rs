//! Download host allowlisting.
//!
//! A `.mrpack` is a manifest of arbitrary URLs that a trusted process fetches unattended. Without
//! a host restriction, a pack author (or anyone who can modify a pack in transit or at rest)
//! could point `downloads[]` at infrastructure they control, or at an address inside the
//! operator's own network. Modrinth therefore restricts pack downloads to a small set of hosts,
//! and hopper enforces that rather than trusting the manifest.
//!
//! Two properties are easy to get wrong and are handled explicitly here:
//!
//! * **Redirects.** Checking only the URL written in the manifest is not enough — an allowed
//!   host that responds `302` to an arbitrary location would bypass the list entirely. Every hop
//!   must be re-checked, which is why [`HostAllowlist::check`] is designed to be called per hop
//!   rather than once per download.
//! * **Separate trust domains.** Pack content and JDK downloads come from different places and
//!   carry different risk. They get separate lists rather than one union, so a compromise of the
//!   pack surface cannot reach the JVM surface.

use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("refusing to download from {host:?}, which is not an allowed host for {domain}")]
    NotAllowed { host: String, domain: &'static str },
    #[error("refusing to download over {scheme:?}; only https is allowed")]
    NotHttps { scheme: String },
    #[error("refusing a URL that embeds credentials")]
    HasCredentials,
    #[error("URL has no host")]
    NoHost,
}

/// Hosts a `.mrpack` may reference. Matches Modrinth's published allowlist.
pub const PACK_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "github.com",
    "raw.githubusercontent.com",
    "gitlab.com",
];

/// Hosts JDK archives and loader installers may come from. Deliberately disjoint from
/// [`PACK_HOSTS`] — these are different trust domains and must not be merged.
pub const RUNTIME_HOSTS: &[&str] = &[
    // Mojang
    "piston-meta.mojang.com",
    "piston-data.mojang.com",
    "launchermeta.mojang.com",
    "launcher.mojang.com",
    "resources.download.minecraft.net",
    // Loaders
    "meta.fabricmc.net",
    "maven.fabricmc.net",
    "maven.quiltmc.org",
    "maven.neoforged.net",
    "maven.minecraftforge.net",
    "files.minecraftforge.net",
    "repo1.maven.org",
    // JVMs
    "download.oracle.com",
    "api.adoptium.net",
    "github.com",
];

/// Registry API hosts. Separate from [`PACK_HOSTS`] because an API that tells us what to
/// download is a different concern from the hosts we will download from.
pub const API_HOSTS: &[&str] = &["api.modrinth.com", "staging-api.modrinth.com"];

/// Hosts reachable only as a redirect from a specific host, never as a URL a pack declares.
///
/// GitHub serves `github.com/<repo>/releases/download/...` by redirecting to its asset host.
/// That host serves only release assets, which `github.com` already denotes, so following the
/// redirect widens nothing. Accepting it as a declared URL would go beyond Modrinth's published
/// list, so it is not.
pub const REDIRECT_TARGETS: &[(&str, &str)] = &[
    ("github.com", "release-assets.githubusercontent.com"),
    // Where GitHub redirected before release-assets existed; older links may still go here.
    ("github.com", "objects.githubusercontent.com"),
];

/// CurseForge's API. Alone in its own list because requests to it carry the operator's API key,
/// and a key-bearing client must not be able to follow a redirect anywhere else.
pub const CURSEFORGE_API_HOSTS: &[&str] = &["api.curseforge.com"];

/// Where CurseForge serves files from. Never sent the API key.
pub const CURSEFORGE_CDN_HOSTS: &[&str] = &["edge.forgecdn.net", "mediafilez.forgecdn.net"];

#[derive(Debug, Clone)]
pub struct HostAllowlist {
    domain: &'static str,
    hosts: Vec<String>,
    /// `(from, to)`: `to` is allowed only as a redirect hop out of `from`.
    redirects: Vec<(String, String)>,
}

impl HostAllowlist {
    /// GTNH publishes its own server bundles and pinned GitHub/CurseForge assets.
    pub fn gtnh() -> Self {
        Self::new(
            "GTNH downloads",
            &[
                "downloads.gtnewhorizons.com",
                "api.github.com",
                "github.com",
                "raw.githubusercontent.com",
                "mediafilez.forgecdn.net",
                "edge.forgecdn.net",
                "files.vexatos.com",
                "nexus.gtnewhorizons.com",
            ],
        )
        .with_redirects(&[
            ("github.com", "release-assets.githubusercontent.com"),
            ("github.com", "objects.githubusercontent.com"),
            ("api.github.com", "release-assets.githubusercontent.com"),
        ])
    }
    pub fn packs() -> Self {
        Self::new("pack downloads", PACK_HOSTS).with_redirects(REDIRECT_TARGETS)
    }

    pub fn runtimes() -> Self {
        Self::new("runtime downloads", RUNTIME_HOSTS).with_redirects(REDIRECT_TARGETS)
    }

    /// API calls plus the CDN they hand out URLs for.
    pub fn api() -> Self {
        let mut hosts: Vec<&str> = API_HOSTS.to_vec();
        hosts.extend_from_slice(PACK_HOSTS);
        Self::new("registry API", &hosts).with_redirects(REDIRECT_TARGETS)
    }

    /// CurseForge's API host and nothing else. See [`CURSEFORGE_API_HOSTS`].
    pub fn curseforge_api() -> Self {
        Self::new("CurseForge API", CURSEFORGE_API_HOSTS)
    }

    /// CurseForge file downloads.
    pub fn curseforge_files() -> Self {
        Self::new("CurseForge downloads", CURSEFORGE_CDN_HOSTS)
    }

    pub fn new(domain: &'static str, hosts: &[&str]) -> Self {
        Self {
            domain,
            hosts: hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
            redirects: Vec::new(),
        }
    }

    /// Permit redirect-only targets. A pair applies only when its `from` host is itself on
    /// this list, so a target never becomes reachable from a host this list does not trust.
    pub fn with_redirects(mut self, pairs: &[(&str, &str)]) -> Self {
        for (from, to) in pairs {
            let from = from.to_ascii_lowercase();
            if self.hosts.contains(&from) {
                self.redirects.push((from, to.to_ascii_lowercase()));
            }
        }
        self
    }

    /// Check one redirect hop: `to` must be allowed outright, or be a redirect-only target of
    /// the host that sent the redirect.
    pub fn check_redirect(&self, from: &Url, to: &Url) -> Result<(), HostError> {
        let err = match self.check(to) {
            Ok(()) => return Ok(()),
            Err(e @ HostError::NotAllowed { .. }) => e,
            Err(other) => return Err(other),
        };
        let from_host = from.host_str().unwrap_or_default().to_ascii_lowercase();
        let to_host = to.host_str().unwrap_or_default().to_ascii_lowercase();
        if self
            .redirects
            .iter()
            .any(|(f, t)| *f == from_host && *t == to_host)
        {
            Ok(())
        } else {
            Err(err)
        }
    }

    /// Permit an extra host. For self-hosted Modrinth instances and mirrors — an explicit
    /// operator decision, never something a pack can ask for.
    pub fn allow(&mut self, host: &str) {
        self.hosts.push(host.to_ascii_lowercase());
    }

    /// Check one URL. Call this for **every** redirect hop, not once per download.
    pub fn check(&self, url: &Url) -> Result<(), HostError> {
        if url.scheme() != "https" {
            return Err(HostError::NotHttps {
                scheme: url.scheme().to_owned(),
            });
        }
        // `https://cdn.modrinth.com@evil.test/` parses with host `evil.test`; rejecting
        // credentials outright removes a class of look-alike URLs from review.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(HostError::HasCredentials);
        }
        let host = url
            .host_str()
            .ok_or(HostError::NoHost)?
            .to_ascii_lowercase();

        // Exact match only. A suffix check would accept `cdn.modrinth.com.evil.test`.
        if self.hosts.contains(&host) {
            Ok(())
        } else {
            Err(HostError::NotAllowed {
                host,
                domain: self.domain,
            })
        }
    }

    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn github_asset_hosts_are_reachable_only_by_redirect_from_github() {
        let gh = u("https://github.com/o/r/releases/download/v1/mod.jar");
        let asset = u(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1?sig=x",
        );
        let old =
            u("https://objects.githubusercontent.com/github-production-release-asset-2e65be/1");

        for a in [
            HostAllowlist::packs(),
            HostAllowlist::runtimes(),
            HostAllowlist::api(),
        ] {
            // Never as a declared URL.
            assert!(a.check(&asset).is_err());
            assert!(a.check(&old).is_err());
            // Only as a hop out of github.com.
            assert!(a.check_redirect(&gh, &asset).is_ok());
            assert!(a.check_redirect(&gh, &old).is_ok());
            assert!(
                a.check_redirect(&u("https://cdn.modrinth.com/x"), &asset)
                    .is_err(),
                "a redirect from anywhere else does not qualify"
            );
            // A redirect never loosens https or the credential check.
            assert!(
                a.check_redirect(&gh, &u("http://release-assets.githubusercontent.com/x"))
                    .is_err()
            );
        }
    }

    #[test]
    fn a_redirect_pair_needs_its_source_host_on_the_list() {
        let a = HostAllowlist::curseforge_api().with_redirects(REDIRECT_TARGETS);
        assert!(
            a.check_redirect(
                &u("https://github.com/x"),
                &u("https://release-assets.githubusercontent.com/x")
            )
            .is_err()
        );
    }

    #[test]
    fn the_key_bearing_curseforge_client_can_reach_only_the_api() {
        let a = HostAllowlist::curseforge_api();
        assert!(a.check(&u("https://api.curseforge.com/v1/mods/1")).is_ok());
        // Its CDN is a different list: a redirect there from a keyed client must be refused.
        assert!(
            a.check(&u("https://edge.forgecdn.net/files/1/2/a.zip"))
                .is_err()
        );
        assert!(a.check(&u("https://cdn.modrinth.com/x")).is_err());

        let files = HostAllowlist::curseforge_files();
        assert!(
            files
                .check(&u("https://edge.forgecdn.net/files/1/2/a.zip"))
                .is_ok()
        );
        assert!(
            files
                .check(&u("https://mediafilez.forgecdn.net/files/1/2/a.zip"))
                .is_ok()
        );
        assert!(
            files
                .check(&u("https://api.curseforge.com/v1/mods/1"))
                .is_err()
        );
    }

    #[test]
    fn accepts_the_published_pack_hosts() {
        let a = HostAllowlist::packs();
        for url in [
            "https://cdn.modrinth.com/data/AANobbMI/versions/x/sodium.jar",
            "https://github.com/org/repo/releases/download/v1/mod.jar",
            "https://raw.githubusercontent.com/org/repo/main/mod.jar",
            "https://gitlab.com/org/repo/-/raw/main/mod.jar",
        ] {
            assert!(a.check(&u(url)).is_ok(), "should accept {url}");
        }
    }

    #[test]
    fn rejects_unlisted_hosts() {
        let a = HostAllowlist::packs();
        assert!(matches!(
            a.check(&u("https://evil.test/payload.jar")).unwrap_err(),
            HostError::NotAllowed { .. }
        ));
    }

    #[test]
    fn matching_is_exact_not_suffix() {
        // The bug a naive `ends_with` check would introduce.
        let a = HostAllowlist::packs();
        for url in [
            "https://cdn.modrinth.com.evil.test/x.jar",
            "https://notgithub.com/x.jar",
            "https://evil.test/cdn.modrinth.com/x.jar",
        ] {
            assert!(a.check(&u(url)).is_err(), "should reject {url}");
        }
    }

    #[test]
    fn subdomains_are_not_implicitly_trusted() {
        let a = HostAllowlist::packs();
        assert!(a.check(&u("https://pages.github.com/x.jar")).is_err());
    }

    #[test]
    fn rejects_plaintext_and_non_http_schemes() {
        let a = HostAllowlist::packs();
        assert!(matches!(
            a.check(&u("http://cdn.modrinth.com/x.jar")).unwrap_err(),
            HostError::NotHttps { .. }
        ));
        assert!(a.check(&u("file:///etc/passwd")).is_err());
        assert!(a.check(&u("ftp://cdn.modrinth.com/x.jar")).is_err());
    }

    #[test]
    fn rejects_embedded_credentials() {
        // Parses with host `evil.test`, but reads like the CDN at a glance.
        let a = HostAllowlist::packs();
        assert!(matches!(
            a.check(&u("https://cdn.modrinth.com@evil.test/x.jar"))
                .unwrap_err(),
            HostError::HasCredentials
        ));
    }

    #[test]
    fn host_comparison_ignores_case() {
        let a = HostAllowlist::packs();
        assert!(a.check(&u("https://CDN.Modrinth.COM/x.jar")).is_ok());
    }

    #[test]
    fn pack_and_runtime_domains_stay_separate() {
        // A compromise of the pack surface must not reach the JVM surface, and vice versa.
        let packs = HostAllowlist::packs();
        let runtimes = HostAllowlist::runtimes();
        assert!(
            packs
                .check(&u("https://download.oracle.com/graalvm/21/latest/x.tar.gz"))
                .is_err()
        );
        assert!(
            runtimes
                .check(&u("https://cdn.modrinth.com/data/x/sodium.jar"))
                .is_err()
        );
    }

    #[test]
    fn runtime_hosts_cover_every_vendor_we_fetch_from() {
        let a = HostAllowlist::runtimes();
        for url in [
            "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
            "https://meta.fabricmc.net/v2/versions/loader/26.3",
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml",
            "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json",
            "https://download.oracle.com/graalvm/21/latest/graalvm-jdk-21_linux-x64_bin.tar.gz",
            "https://api.adoptium.net/v3/assets/latest/21/hotspot",
        ] {
            assert!(a.check(&u(url)).is_ok(), "should accept {url}");
        }
    }

    #[test]
    fn the_api_allowlist_covers_the_api_and_the_cdn_it_points_at() {
        let a = HostAllowlist::api();
        assert!(
            a.check(&u("https://api.modrinth.com/v2/project/sodium"))
                .is_ok()
        );
        // Version metadata hands out CDN URLs, so both have to be reachable from one client.
        assert!(
            a.check(&u("https://cdn.modrinth.com/data/X/pack.mrpack"))
                .is_ok()
        );
        assert!(a.check(&u("https://evil.test/x")).is_err());
    }

    #[test]
    fn operators_can_add_a_mirror() {
        let mut a = HostAllowlist::packs();
        assert!(a.check(&u("https://mirror.internal/x.jar")).is_err());
        a.allow("mirror.internal");
        assert!(a.check(&u("https://mirror.internal/x.jar")).is_ok());
    }
}
