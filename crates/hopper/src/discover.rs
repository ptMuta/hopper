//! Typed registry lookups for prompts: what packs exist and which releases they have.
//! Built for a person waiting on the answer: one attempt, no backoff, short deadlines.
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hopper_core::net::{HostAllowlist, HttpClient};
use serde::Deserialize;

use crate::interface::Provider;

/// One pack a search found, bound to exactly one provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub provider: Provider,
    /// `--pack`: a Modrinth slug or CurseForge project ID.
    pub pack: Option<String>,
    pub file: Option<PathBuf>,
    pub url: Option<String>,
    /// A starting point for the instance name.
    pub slug: String,
}

impl Pick {
    pub fn gtnh() -> Self {
        Pick {
            provider: Provider::Gtnh,
            pack: None,
            file: None,
            url: None,
            slug: "gtnh".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub pick: Pick,
    pub title: String,
    pub minecraft: Option<String>,
    pub loader: Option<String>,
    pub downloads: u64,
}

/// A release of a pack, newest first in any list.
#[derive(Debug, Clone)]
pub struct Release {
    /// `--pack-version`.
    pub id: String,
    pub label: String,
    pub stable: bool,
    pub minecraft: Option<String>,
    pub published: String,
}

pub struct Discovery {
    modrinth: HttpClient,
    curseforge: Option<HttpClient>,
    gtnh: HttpClient,
}

fn once(client: HttpClient) -> HttpClient {
    client.with_retry(hopper_core::api::ratelimit::RetryPolicy {
        max_attempts: 1,
        ..Default::default()
    })
}

fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// `412k`, `1.2M`.
pub fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

impl Hit {
    /// `modrinth · 1.21.1 fabric · 412k`
    pub fn hint(&self) -> String {
        let sep = crate::prompt::sep();
        let mut parts = vec![format!("{:?}", self.pick.provider).to_lowercase()];
        let target: Vec<&str> = [self.minecraft.as_deref(), self.loader.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if !target.is_empty() {
            parts.push(target.join(" "));
        }
        if self.downloads > 0 {
            parts.push(count(self.downloads));
        }
        parts.join(&format!(" {sep} "))
    }
}

#[derive(Deserialize)]
struct ModrinthHits {
    hits: Vec<ModrinthHit>,
}
#[derive(Deserialize)]
struct ModrinthHit {
    slug: String,
    title: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Deserialize)]
struct CurseForgeHits {
    data: Vec<CurseForgeHit>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurseForgeHit {
    id: u64,
    slug: String,
    name: String,
    #[serde(default)]
    download_count: f64,
    #[serde(default)]
    latest_files_indexes: Vec<CurseForgeIndex>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurseForgeIndex {
    game_version: String,
    #[serde(default)]
    mod_loader: Option<u32>,
}

const LOADERS: [&str; 4] = ["fabric", "neoforge", "forge", "quilt"];

impl Discovery {
    pub fn new() -> Result<Self> {
        let agent = crate::user_agent();
        let curseforge = crate::curseforge::api_key(None).and_then(|key| {
            HttpClient::with_secret_header(
                &agent,
                HostAllowlist::curseforge_api(),
                "x-api-key",
                &key,
            )
            .ok()
            .map(once)
        });
        Ok(Self {
            modrinth: once(HttpClient::new(&agent, HostAllowlist::api())?),
            curseforge,
            gtnh: once(HttpClient::new(&agent, HostAllowlist::gtnh())?),
        })
    }

    pub fn has_curseforge(&self) -> bool {
        self.curseforge.is_some()
    }

    async fn modrinth(&self, query: &str) -> Result<Vec<Hit>> {
        let order = if query.is_empty() {
            "downloads"
        } else {
            "relevance"
        };
        let url = format!(
            "https://api.modrinth.com/v2/search?query={}&facets={}&index={order}&limit=10",
            encode(query),
            encode("[[\"project_type:modpack\"]]")
        );
        let found: ModrinthHits = self.modrinth.get_json(&url).await?;
        Ok(found
            .hits
            .into_iter()
            .map(|h| Hit {
                minecraft: h.versions.last().cloned(),
                loader: LOADERS
                    .iter()
                    .find(|l| h.categories.iter().any(|c| c == *l))
                    .map(|l| l.to_string()),
                downloads: h.downloads,
                title: h.title,
                pick: Pick {
                    provider: Provider::Modrinth,
                    pack: Some(h.slug.clone()),
                    file: None,
                    url: None,
                    slug: h.slug,
                },
            })
            .collect())
    }

    async fn curseforge(&self, query: &str) -> Result<Vec<Hit>> {
        let Some(client) = &self.curseforge else {
            return Ok(vec![]);
        };
        let url = format!(
            "https://api.curseforge.com/v1/mods/search?gameId=432&classId=4471&searchFilter={}&sortField={}&sortOrder=desc&pageSize=10",
            encode(query),
            if query.is_empty() { 6 } else { 2 }
        );
        let found: CurseForgeHits = client.get_json(&url).await?;
        Ok(found
            .data
            .into_iter()
            .map(|h| {
                let index = h.latest_files_indexes.first();
                Hit {
                    minecraft: index.map(|i| i.game_version.clone()),
                    loader: index.and_then(|i| match i.mod_loader {
                        Some(1) => Some("forge".into()),
                        Some(4) => Some("fabric".into()),
                        Some(5) => Some("quilt".into()),
                        Some(6) => Some("neoforge".into()),
                        _ => None,
                    }),
                    downloads: h.download_count as u64,
                    title: h.name,
                    pick: Pick {
                        provider: Provider::Curseforge,
                        // Project IDs are stable where slugs can change.
                        pack: Some(h.id.to_string()),
                        file: None,
                        url: None,
                        slug: h.slug,
                    },
                }
            })
            .collect())
    }

    /// Packs from one provider, or from every searchable one interleaved so neither
    /// registry's download counts drown out the other's.
    pub async fn search(&self, provider: Option<Provider>, query: &str) -> Result<Vec<Hit>> {
        match provider {
            Some(Provider::Modrinth) => self.modrinth(query).await,
            Some(Provider::Curseforge) => self.curseforge(query).await,
            Some(_) => Ok(vec![]),
            None => {
                let (m, c) = tokio::join!(self.modrinth(query), self.curseforge(query));
                let (m, c) = match (m, c) {
                    (Err(e), Err(_)) => return Err(e),
                    (m, c) => (m.unwrap_or_default(), c.unwrap_or_default()),
                };
                let mut merged = vec![];
                let (mut m, mut c) = (m.into_iter(), c.into_iter());
                loop {
                    match (m.next(), c.next()) {
                        (None, None) => break,
                        (a, b) => merged.extend(a.into_iter().chain(b)),
                    }
                }
                Ok(merged)
            }
        }
    }

    /// Releases of a registry pack, newest first.
    pub async fn releases(&self, provider: Provider, pack: &str) -> Result<Vec<Release>> {
        use hopper_core::api::{curseforge, modrinth::VersionType};
        Ok(match provider {
            Provider::Modrinth => hopper_core::api::client::fetch_versions(&self.modrinth, pack)
                .await?
                .into_iter()
                .map(|v| Release {
                    stable: v.version_type == Some(VersionType::Release),
                    label: v.version_number,
                    id: v.id.to_string(),
                    minecraft: v.game_versions.first().cloned(),
                    published: v.date_published.unwrap_or_default(),
                })
                .collect(),
            Provider::Curseforge => {
                let client = self
                    .curseforge
                    .as_ref()
                    .context("CURSEFORGE_API_KEY is required")?;
                let project = curseforge::find_modpack(client, pack).await?;
                let mut files: Vec<Release> = curseforge::list_files(client, project.id)
                    .await?
                    .into_iter()
                    .filter(|f| !f.is_server_pack && f.is_available != Some(false))
                    .map(|f| Release {
                        stable: f.release() == Some(curseforge::ReleaseType::Release),
                        label: f.label().to_owned(),
                        id: f.id.to_string(),
                        minecraft: f.game_versions.first().cloned(),
                        published: f.file_date,
                    })
                    .collect();
                files.sort_by(|a, b| b.published.cmp(&a.published));
                files
            }
            Provider::Gtnh => {
                let catalog: BTreeMap<String, crate::gtnh::Release> =
                    self.gtnh.get_json(crate::gtnh::CATALOG).await?;
                let mut releases: Vec<Release> = catalog
                    .into_iter()
                    .filter(|(name, _)| name.chars().next().is_some_and(|c| c.is_ascii_digit()))
                    .map(|(name, r)| Release {
                        stable: r.title == "Stable release",
                        label: name.clone(),
                        id: name,
                        minecraft: Some("1.7.10".into()),
                        published: r.date,
                    })
                    .collect();
                releases.sort_by(|a, b| {
                    b.published
                        .cmp(&a.published)
                        .then_with(|| crate::gtnh::natural(&b.id).cmp(&crate::gtnh::natural(&a.id)))
                });
                releases
            }
            Provider::Mrpack => vec![],
        })
    }
}

/// What typed text means without a registry: a local `.mrpack`, an `https://` pack URL.
pub fn literal(text: &str) -> Option<Pick> {
    let text = text.trim();
    if text.starts_with("https://") {
        let url = url::Url::parse(text).ok()?;
        HostAllowlist::packs().check(&url).ok()?;
        let slug = url
            .path_segments()?
            .next_back()?
            .trim_end_matches(".mrpack")
            .to_owned();
        return Some(Pick {
            provider: Provider::Mrpack,
            pack: None,
            file: None,
            url: Some(text.to_owned()),
            slug,
        });
    }
    let looks_like_path = text.starts_with(['/', '.', '~']) || text.ends_with(".mrpack");
    if looks_like_path {
        let path = match text.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => PathBuf::from(text),
        };
        let path = path.canonicalize().ok().filter(|p| p.is_file())?;
        let slug = path.file_stem()?.to_string_lossy().into_owned();
        return Some(Pick {
            provider: Provider::Mrpack,
            pack: None,
            file: Some(path),
            url: None,
            slug,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_reads_paths_and_allowed_urls() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my-pack.mrpack");
        std::fs::write(&file, b"x").unwrap();
        let pick = literal(file.to_str().unwrap()).unwrap();
        assert_eq!(pick.provider, Provider::Mrpack);
        assert_eq!(pick.slug, "my-pack");
        assert!(literal("/nonexistent/x.mrpack").is_none());
        assert!(literal("atm9").is_none());
        let url = literal("https://cdn.modrinth.com/data/x/versions/y/pack.mrpack").unwrap();
        assert_eq!(url.slug, "pack");
        assert!(literal("https://evil.example/pack.mrpack").is_none());
    }

    #[test]
    fn counts_read_at_a_glance() {
        assert_eq!(count(999), "999");
        assert_eq!(count(412_345), "412k");
        assert_eq!(count(1_234_567), "1.2M");
    }
}
